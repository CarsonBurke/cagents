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
The installer and the commands below use an authenticated GitHub CLI account.
You can also download release assets directly from the releases page; tagged
builds stamp the repository into the released copy of `install.sh` automatically.

## Requirements

The supported release target is **Linux x86_64 with glibc**, built on Ubuntu
22.04, running in a **Wayland** session. X11, musl, ARM, and other operating
systems or package formats are not supported yet. A normal graphical login
provides an absolute `XDG_RUNTIME_DIR`, which cagents uses for the private
daemon socket. The archive installer rejects glibc versions older than 2.35
and unknown version results before installing or updating. Existing installations
can still be removed on older glibc systems.

The `.deb` declares `git (>= 2.36)`, `libc6 (>= 2.35)`, Fontconfig, FreeType,
GCC/libstdc++, Wayland client, xkbcommon, and zlib runtime dependencies. The
full package also requires `libegl1` for the integrated browser; the browserless
package does not add this dependency. The portable archive needs equivalent
libraries. Git 2.36 or newer is required for
NUL-delimited worktree discovery, so Ubuntu 22.04's default Git 2.34 must be
upgraded separately. Hosted terminals need a shell, PTY support, and Linux
`/proc`.

Interface text uses the desktop's installed fonts with Fontconfig fallbacks.
Install Open Sans and Noto Sans Mono if they are not already available.

The GitHub CLI, `gh`, is optional for local-only use but required for GitHub
and pull-request features and for the release installer. Install it from your
distribution and authenticate as the same desktop user:

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
  repository='github.com/CarsonBurke/cagents'
  gh auth status --hostname github.com
  tag="$(gh release view --repo "$repository" --json tagName --jq .tagName)"
  case "$tag" in v[0-9]*) ;; *) echo 'Expected a versioned release tag' >&2; exit 1 ;; esac
  version="${tag#v}"
  download_dir="$(mktemp -d)"
  trap 'rm -rf "$download_dir"' 0
  trap 'exit 1' HUP INT TERM
  archive="cagents-${version}-x86_64-unknown-linux-gnu.tar.gz"

  gh release download "$tag" --repo "$repository" \
    --pattern install.sh --pattern SHA256SUMS --pattern "$archive" \
    --dir "$download_dir"
  (
    cd "$download_dir"
    awk -v archive="$archive" \
      '$2 == "install.sh" { print; installer++ } $2 == archive { print; payload++ }
       END { exit(installer != 1 || payload != 1) }' \
      SHA256SUMS > selected-checksums
    sha256sum --strict --check selected-checksums
  )

  CAGENTS_REPOSITORY='CarsonBurke/cagents' \
  CAGENTS_ARCHIVE="$download_dir/$archive" \
  CAGENTS_CHECKSUMS="$download_dir/SHA256SUMS" \
    sh "$download_dir/install.sh" --version "$version"
)
```

To inspect the installer first, add `less "$download_dir/install.sh"` just
before the `CAGENTS_REPOSITORY` line. To pin a particular version, replace the
`tag=` lookup with that exact tag, for example `tag=v0.1.2`.

SHA-256 confirms that the files match the release manifest. It does not protect
against a compromised repository or release, so inspect the tag, release, and
installer according to your own trust requirements.

The installer uses your existing `gh` authentication both to select the latest
release and to download pinned assets. It never copies tokens into its state,
URLs, or command arguments. The released installer requires authentication for
`CarsonBurke/cagents`, even though its releases are public, and fails before
changing anything if authentication is missing. Supplying both
`CAGENTS_ARCHIVE` and `CAGENTS_CHECKSUMS` keeps installation offline. Anonymous
HTTPS downloads are only a fallback for explicitly selected public
repositories.

Run `sh install.sh --help` for every option.

## Convenience install

This shorter flow uses the same GitHub account but runs the downloaded
installer without inspecting it or checking its checksum yourself. Use it only
if you trust the repository and release. It resolves the latest tag once and
uses it for both the installer and the archive:

```sh
(
  gh auth status --hostname github.com || exit 1
  tag="$(gh release view --repo github.com/CarsonBurke/cagents --json tagName --jq .tagName)" || exit 1
  case "$tag" in v*) ;; *) exit 1 ;; esac
  download_dir="$(mktemp -d)" || exit 1
  trap 'rm -rf "$download_dir"' 0
  trap 'exit 1' HUP INT TERM
  gh release download "$tag" --repo github.com/CarsonBurke/cagents \
    --pattern install.sh --dir "$download_dir" &&
    sh "$download_dir/install.sh" --version "${tag#v}"
)
```

To pin a version instead, replace the `tag=` lookup with a fixed version and
download that tag's installer:

```sh
(
  gh auth status --hostname github.com || exit 1
  VERSION=0.1.2
  download_dir="$(mktemp -d)" || exit 1
  trap 'rm -rf "$download_dir"' 0
  trap 'exit 1' HUP INT TERM
  gh release download "v${VERSION}" --repo github.com/CarsonBurke/cagents \
    --pattern install.sh --dir "$download_dir" &&
    sh "$download_dir/install.sh" --version "$VERSION"
)
```

## Debian packages

Download the package and checksum manifest, verify the package, then let APT
resolve its shared-library dependencies:

```sh
REPOSITORY='CarsonBurke/cagents'
VERSION='0.1.2'
gh release download "v${VERSION}" --repo "github.com/${REPOSITORY}" \
  --pattern "cagents_${VERSION}_amd64.deb" --pattern SHA256SUMS
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
gh release download "v${VERSION}" --repo "github.com/${REPOSITORY}" \
  --pattern "cagents-browserless_${VERSION}_amd64.deb" --pattern SHA256SUMS-browserless
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
executable. Keep its `bin` directory intact: `cagents`, `cagents-ui`, and
`cagentsd` must stay together.

```sh
REPOSITORY='CarsonBurke/cagents'
VERSION='0.1.2'
gh release download "v${VERSION}" --repo "github.com/${REPOSITORY}" \
  --pattern "cagents-${VERSION}-x86_64-unknown-linux-gnu.tar.gz" --pattern SHA256SUMS
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

The standard installation starts its per-user `cagentsd` immediately and
registers it for graphical-login autostart. See [Daemon and
upgrades](daemon.md) for how the daemon, sessions, and upgrades interact.

## Updating

### In-app updates

Full self-managed (`install.sh`) installations, including packages built
locally with a `+local` version, check authenticated GitHub releases at startup
and every six hours. A newer stable version appears in a notice above the
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
including local builds, use this same flow. Source trees and OS packages retain
their own update method; browserless builds do not download full releases.

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
is still available; the diagnostic files are kept.

### From the command line

Rerun the installer for a compatible update: the existing daemon and its live
PTYs keep running. By default the installer verifies the staged CLI against the
running daemon before switching `current`. An incompatible build is installed
without replacing the running daemon; saved windows reopen with that daemon's
matching UI. Finish the staged update in the application or run
`cagents daemon upgrade`. This command displays the same impact, asks for
approval, and waits for completion. `--yes` skips the prompt; `--interrupt`
additionally permits active native turns to stop.

To make a verified build available without saving or closing running windows,
append `--stage-only` to the installer invocation. It publishes the complete
candidate while preserving unsaved windows and the running daemon; it does not
start, stop or checkpoint them. The existing application's update control then
offers the reviewed apply/reopen flow. Resolve any save blockers before applying:
staging makes the candidate available but does not complete the application handoff.
This option cannot be combined with restart, relaunch, uninstall or purge options.

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
keeps settings, sessions, transcripts, and caches:

```sh
"${XDG_DATA_HOME:-$HOME/.local/share}/cagents-install/current/bin/cagents-uninstall"
```

To remove both program files and all cagents user data:

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
first.

Remove a Debian installation with:

```sh
sudo apt remove cagents             # full package
sudo apt remove cagents-browserless # browserless package
```

APT removal and purge do not delete files in users' XDG directories.
