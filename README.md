# cagents

cagents is a native Wayland desktop application for organizing coding-agent sessions, repositories, Git worktrees, terminals, and embedded browser surfaces in one workspace. It also provides a scriptable CLI for the persistent terminal daemon.

> [!IMPORTANT]
> This repository is hosted privately at `CarsonBurke/cagents`. Release downloads and the curl installer require access to that private repository; tagged release builds stamp the repository into the released copy of `install.sh` automatically.

## Platform and runtime requirements

The initial supported release target is **Linux x86_64 with glibc**, built on Ubuntu 22.04, in a **Wayland** session. X11, musl, ARM, and other operating systems or package formats are not currently supported. A normal graphical login should provide an absolute `XDG_RUNTIME_DIR`; cagents uses it for the private daemon socket.

The `.deb` declares `git`, `libc6 (>= 2.35)`, Fontconfig, FreeType, GCC/libstdc++, Wayland client, xkbcommon, and zlib runtime dependencies. The portable archive needs equivalent libraries. A shell, PTY support, and Linux `/proc` are required for hosted terminals.

The GitHub CLI, `gh`, is optional for local-only use but required for cagents GitHub and pull-request operations. Install it with your distribution, then authenticate as the same desktop user:

```sh
gh auth login
gh auth status
```

cagents delegates authentication to `gh`; it does not ask for or store a GitHub token itself.

## Run from source

Development builds require Rust 1.93, Zig 0.15.2, and the native dependencies used by the release workflow. On Ubuntu 22.04:

```sh
sudo apt-get update
sudo apt-get install --yes --no-install-recommends \
  build-essential clang cmake git libclang-dev libdbus-1-dev \
  libegl1-mesa-dev libfontconfig1-dev libfreetype6-dev libgbm-dev \
  libgl1-mesa-dev libssl-dev libudev-dev libwayland-dev \
  libxkbcommon-dev ninja-build pkg-config python3 xz-utils
rustup toolchain install 1.93.0 --profile minimal
rustup default 1.93.0
```

Install Zig 0.15.2 from [ziglang.org](https://ziglang.org/download/) and ensure `zig` is on `PATH`. Build all three executables before running so the UI and CLI can find their sibling `cagentsd`:

```sh
cargo build --locked --bins
cargo run --locked --bin cagents-ui
```

Running only one binary from an unrelated directory is not a supported layout; `cagents`, `cagents-ui`, and `cagentsd` must remain together.

## Install a release

All examples in this section become usable only after canonical hosting exists. Set the real repository and choose a published version:

```sh
REPOSITORY='CarsonBurke/cagents'
VERSION='0.1.0'
```

### Recommended: download, verify, inspect, then install

This flow pins an exact release, verifies both downloaded files against its checksum manifest, lets you read the installer before executing it, and supplies the already-verified archive back to the installer:

```sh
download_dir="$(mktemp -d)"
base_url="https://github.com/${REPOSITORY}/releases/download/v${VERSION}"
archive="cagents-${VERSION}-x86_64-unknown-linux-gnu.tar.gz"

curl --proto '=https' --tlsv1.2 --fail --location \
  "${base_url}/install.sh" --output "${download_dir}/install.sh"
curl --proto '=https' --tlsv1.2 --fail --location \
  "${base_url}/SHA256SUMS" --output "${download_dir}/SHA256SUMS"
curl --proto '=https' --tlsv1.2 --fail --location \
  "${base_url}/${archive}" --output "${download_dir}/${archive}"

(
  cd "$download_dir"
  sha256sum --ignore-missing --strict --check SHA256SUMS
)
less "${download_dir}/install.sh"

CAGENTS_REPOSITORY="$REPOSITORY" \
CAGENTS_ARCHIVE="${download_dir}/${archive}" \
CAGENTS_CHECKSUMS="${download_dir}/SHA256SUMS" \
  sh "${download_dir}/install.sh" --version "$VERSION"
rm -rf "$download_dir"
```

SHA-256 confirms that the files match the release manifest; it does not protect against compromise of the GitHub repository or release itself. Inspect the tag, release, and installer according to your trust requirements.

The installer never uses `sudo` and refuses to run as root. It installs an immutable version below `${XDG_DATA_HOME:-$HOME/.local/share}/cagents-install/`, switches the `current` symlink atomically, creates desktop metadata links under `${XDG_DATA_HOME:-$HOME/.local/share}`, and exposes only `$HOME/.local/bin/cagents` on `PATH`. Ensure `$HOME/.local/bin` is in `PATH`.

### Convenience curl install

This is shorter but executes downloaded code without inspecting or independently authenticating it. Use it only if you trust the GitHub repository and release:

```sh
(
  installer="$(mktemp)" || exit 1
  trap 'rm -f "$installer"' 0
  trap 'exit 1' HUP INT TERM
  curl --proto '=https' --tlsv1.2 --fail --silent --show-error --location \
    "https://github.com/CarsonBurke/cagents/releases/latest/download/install.sh" \
    --output "$installer" &&
    sh "$installer"
)
```

A released installer is stamped with its actual GitHub repository. To pin a version through the convenience flow, pass installer arguments to `sh`:

```sh
(
  installer="$(mktemp)" || exit 1
  trap 'rm -f "$installer"' 0
  trap 'exit 1' HUP INT TERM
  curl --proto '=https' --tlsv1.2 --fail --silent --show-error --location \
    "https://github.com/CarsonBurke/cagents/releases/latest/download/install.sh" \
    --output "$installer" &&
    sh "$installer" --version 0.1.0
)
```

### Debian package

Download the release package and checksum manifest, verify the package, then let APT resolve its shared-library dependencies:

```sh
curl --proto '=https' --tlsv1.2 --fail --location --remote-name \
  "https://github.com/${REPOSITORY}/releases/download/v${VERSION}/cagents_${VERSION}_amd64.deb"
curl --proto '=https' --tlsv1.2 --fail --location --remote-name \
  "https://github.com/${REPOSITORY}/releases/download/v${VERSION}/SHA256SUMS"
sha256sum --ignore-missing --strict --check SHA256SUMS
sudo apt install git "./cagents_${VERSION}_amd64.deb"
```

The Debian package installs the three executables together under `/usr/libexec/cagents`, exposes `/usr/bin/cagents`, and installs the desktop entry, icon, and AppStream metadata system-wide.

### Portable archive

The archive is self-contained as a directory layout, not as a static executable. Keep its `bin` directory intact:

```sh
curl --proto '=https' --tlsv1.2 --fail --location --remote-name \
  "https://github.com/${REPOSITORY}/releases/download/v${VERSION}/cagents-${VERSION}-x86_64-unknown-linux-gnu.tar.gz"
curl --proto '=https' --tlsv1.2 --fail --location --remote-name \
  "https://github.com/${REPOSITORY}/releases/download/v${VERSION}/SHA256SUMS"
sha256sum --ignore-missing --strict --check SHA256SUMS
tar -xzf "cagents-${VERSION}-x86_64-unknown-linux-gnu.tar.gz"
"./cagents-${VERSION}/bin/cagents" ui
```

## Use cagents

Launch **cagents** from the desktop application menu, or launch it from a terminal:

```sh
cagents ui
```

The control CLI includes human-readable output by default and stable JSON envelopes for automation:

```sh
cagents daemon status
cagents terminal list
cagents terminal spawn -- bash -lc 'printf "hello from cagents\\n"'
cagents --json terminal list
cagents daemon stop
```

Run `cagents --help` or `cagents terminal --help` for the complete command set.

## Daemon and user data

There is no service unit, login autostart, or system daemon. A UI or terminal command starts the per-user sibling `cagentsd` on demand. The daemon runs independently after the launching process exits so terminals can survive UI restarts. Stop it explicitly with `cagents daemon stop`; installers and package upgrades also stop an old daemon before switching executables.

cagents follows the XDG base-directory convention:

- Settings: `${XDG_CONFIG_HOME:-$HOME/.config}/cagents/settings.toml`
- Session database: `${XDG_DATA_HOME:-$HOME/.local/share}/cagents/sessions.sqlite3`
- Terminal transcripts: `${XDG_DATA_HOME:-$HOME/.local/share}/cagents/transcripts/`
- Cache, when present: `${XDG_CACHE_HOME:-$HOME/.cache}/cagents/`
- Daemon socket and lock: `$XDG_RUNTIME_DIR/cagents/`

Program files installed by `install.sh` live separately under `${XDG_DATA_HOME:-$HOME/.local/share}/cagents-install/`.

### Settings

`settings.toml` uses the version 2 schema below. The original runtime and layout
keys remain required; newer preferences are optional when reading older files
and use the documented defaults. Unknown keys, unsupported schema versions,
malformed TOML, conflicting shortcuts, and values outside the documented ranges
are rejected.

```toml
schema_version = 2
max_hot_cards = 6
hibernation_delay_ms = 30000
terminal_font_size = 14.0
terminal_font_family = ""
sidebar_width = 280.0
default_column_fraction = 0.72
terminal_scrollback_lines = 10000
link_open_destination = "integrated_browser"
link_open_gesture = "control_click"

[keyboard_shortcuts]
new_terminal = "Ctrl+T"
close_focused_column = "Ctrl+W"
search_projects = "Ctrl+F"
previous_column = "Alt+Q"
next_column = "Alt+W"
```

| Key | Type | Default | Accepted values |
| --- | --- | --- | --- |
| `schema_version` | integer | `2` | exactly `2` |
| `max_hot_cards` | non-negative integer | `6` | `1` through `9223372036854775807` |
| `hibernation_delay_ms` | non-negative integer | `30000` | `0` through `9223372036854775807` milliseconds |
| `terminal_font_size` | float | `14.0` | finite values from `8.0` through `40.0`, inclusive |
| `terminal_font_family` | string | `""` | empty for system monospace, or a non-whitespace family name up to 128 bytes |
| `terminal_scrollback_lines` | non-negative integer | `10000` | `0` through `1000000` lines for newly started terminals |
| `sidebar_width` | float | `280.0` | finite values from `240.0` through `360.0`, inclusive |
| `default_column_fraction` | float | `0.72` | finite values from `0.25` through `1.0`, inclusive |
| `link_open_destination` | string | `"integrated_browser"` | `"integrated_browser"` or `"system_default"` |
| `link_open_gesture` | string | `"control_click"` | `"control_click"` or `"control_shift_click"` |
| `keyboard_shortcuts.*` | string | shown above | a unique chord containing Ctrl, Alt, or Super and one A-Z, 0-9, Left, or Right key; Shift is optional |

Version 1 TOML files are upgraded while loading. Sidebar widths accepted by
version 1 are clamped to the current visible range; other values are preserved.

On first use after upgrading, cagents migrates a sibling `settings.json` only
when `settings.toml` does not exist. It validates the JSON, writes and syncs the
TOML replacement, then makes a best-effort attempt to remove the JSON file. An
existing TOML file always takes precedence; malformed TOML is reported instead
of falling back to JSON. The configuration directory and settings file are
restricted to modes `0700` and `0600`, and symbolic-link destinations are
rejected.

New terminal cards, and `cagents terminal spawn` when no command is supplied,
select the first valid executable shell from the effective user's passwd entry,
`SHELL`, then `/bin/sh`. This resolution applies only when creating a new
default terminal. Explicit CLI commands and launch recipes restored from saved
sessions are used unchanged.

## Uninstall or purge

For an `install.sh` installation, run the installed helper directly. Normal uninstall preserves settings, sessions, transcripts, and caches:

```sh
"${XDG_DATA_HOME:-$HOME/.local/share}/cagents-install/current/bin/cagents-uninstall"
```

To remove both program files and all cagents user data:

```sh
"${XDG_DATA_HOME:-$HOME/.local/share}/cagents-install/current/bin/cagents-uninstall" --purge
```

The downloaded installer offers the equivalent `--uninstall` and `--purge` options, and both scripts refuse root execution. If a custom `--prefix` was used during installation, invoke that prefix's `current/bin/cagents-uninstall` with the same `--prefix`.

Remove a Debian installation with:

```sh
sudo apt remove cagents
```

APT removal and purge do not delete files in the user's XDG directories.

## Releases

Once a canonical GitHub repository is configured, pushing a `v*` tag runs `.github/workflows/release.yml` on Ubuntu 22.04 with Rust 1.93 and Zig 0.15.2. The tag must exactly equal `v` plus the version in `Cargo.toml`; a mismatch fails before package publication. A successful run tests and checks the locked source, validates the desktop and AppStream metadata, calls `scripts/package-linux.sh`, stamps the canonical repository into `dist/install.sh`, regenerates `SHA256SUMS`, and creates a non-draft GitHub release containing:

- `cagents-${VERSION}-x86_64-unknown-linux-gnu.tar.gz`
- `cagents_${VERSION}_amd64.deb`
- `SHA256SUMS`
- `install.sh`

The workflow uses its least-privilege GitHub token for publication through the preinstalled `gh` CLI; it does not require a maintainer's personal `gh` login.

## License

cagents is licensed under the MIT License. See `LICENSE`.
