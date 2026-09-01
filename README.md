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
  libxkbcommon-dev ninja-build pkg-config python3 xdg-utils xz-utils
rustup toolchain install 1.93.0 --profile minimal
rustup default 1.93.0
```

Install Zig 0.15.2 from [ziglang.org](https://ziglang.org/download/) and ensure `zig` is on `PATH`. The terminal host uses Ghostty's native library, so daemon builds require Zig. Ordinary debug builds automatically use the isolated development instance; optimized builds are production unless the Cargo `development` feature is explicitly enabled.

The CLI, UI client, and terminal host have separate Cargo feature boundaries:

```sh
# Control CLI: no UI, Ghostty, SQLite, or Servo graph.
cargo check --locked --no-default-features --bin cagents

# Terminal daemon: Ghostty/PTY host, no UI or Servo graph.
cargo check --locked --no-default-features --features host --bin cagentsd

# Browserless UI client: libcosmic, no Ghostty or Servo graph.
cargo check --locked --no-default-features --features ui --bin cagents-ui

# Integrated UI client: libcosmic and Servo, no Ghostty graph.
cargo check --locked --features integrated-browser --bin cagents-ui
```

Cargo features are package-wide, so combining `ui` and `host` in one `--bins` invocation would undo that isolation. Build a matched browserless release as three feature-disjoint targets:

```sh
cargo build --locked --release --no-default-features --bin cagents
cargo build --locked --release --no-default-features --features host --bin cagentsd
cargo build --locked --release --no-default-features --features ui --bin cagents-ui
```

Browserless builds keep terminals, repositories, worktrees, GitHub pull-request flows, and persistence. New links use the desktop portal and fall back to the system `xdg-open` handler when a portal is unavailable; Debian packages install `xdg-utils`, and portable/source users must provide it for that fallback. A restored browser card retains its saved URL, history, scroll position, and fidelity, shows that the integrated browser is unavailable, and offers **Open in system browser**. Running only one binary from an unrelated directory is not a supported layout; `cagents`, `cagents-ui`, and `cagentsd` must remain together.

Launch the native UI through the development runner:

```sh
scripts/run-development --browserless
scripts/run-development
```

The browserless UI preserves saved browser descriptors and opens their URLs through the system browser instead of compiling Servo. The normal development runner enables `integrated-browser` only for the UI target; the matched CLI and daemon retain the same full-product identity without resolving Servo.

For UI diagnosis, add `--debug`. It enables Rust backtraces and `cagents=debug,cagentsd=debug,warn` tracing; an explicit `RUST_LOG` or `RUST_BACKTRACE` value takes precedence. Use `--fresh-daemon` after changing daemon, terminal-host, or protocol code so the UI cannot reconnect to an older development daemon:

```sh
scripts/run-development --browserless --debug
scripts/run-development --debug --fresh-daemon
scripts/run-development --browserless --debug --log-file /tmp/cagents-ui.log
```

`--log-file` captures UI tracing and panic output in a stable artifact while Cargo diagnostics remain in the invoking terminal. Fresh-daemon mode compiles a feature-matched control CLI, pins and validates the isolated daemon through a Linux pidfd, and signals it without depending on the old wire protocol; stopping it terminates its hosted development terminals. `scripts/run-development` is POSIX shell; the repository contains no Python development tooling. Python remains a native build prerequisite for the full Servo dependency graph.

The runner builds the three sibling executables with separate Cargo feature sets and executes the selected UI through Cargo, so configured target directories and target triples are honored. UI-only compilation never builds Ghostty, and daemon-only compilation never builds libcosmic or Servo. Its `dev-runtime` profile keeps UI dependencies optimized for usable UI performance while the ordinary `dev` profile stays cheap for checks. The development instance has its own application ID, configuration, session database, transcripts, application lease, and `$XDG_RUNTIME_DIR/cagents-development/` daemon, so it can run beside the installed production instance. `cagents`, `cagents-ui`, and `cagentsd` must remain together.

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

The installer never uses `sudo` and refuses to run as root. It installs an immutable version below `${XDG_DATA_HOME:-$HOME/.local/share}/cagents-install/`, switches the `current` symlink atomically, creates desktop metadata links under `${XDG_DATA_HOME:-$HOME/.local/share}`, and exposes only `$HOME/.local/bin/cagents` on `PATH`. The application menu entry uses that installed binary and icon by absolute path, so launching from the desktop does not depend on `PATH`. Terminal use of the `cagents` command still requires `$HOME/.local/bin` on `PATH`.

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

The separately named browserless package can be installed alongside the full package. It uses `/usr/libexec/cagents-browserless`, exposes `/usr/bin/cagents-browserless`, and installs separately identified desktop and AppStream metadata:

```sh
curl --proto '=https' --tlsv1.2 --fail --location --remote-name \
  "https://github.com/${REPOSITORY}/releases/download/v${VERSION}/cagents-browserless_${VERSION}_amd64.deb"
curl --proto '=https' --tlsv1.2 --fail --location --remote-name \
  "https://github.com/${REPOSITORY}/releases/download/v${VERSION}/SHA256SUMS-browserless"
sha256sum --ignore-missing --strict --check SHA256SUMS-browserless
sudo apt install git "./cagents-browserless_${VERSION}_amd64.deb"
cagents-browserless ui
```

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

There is no system daemon or service unit. The standard production installation starts its per-user sibling `cagentsd` immediately and registers it for graphical-login autostart. While that daemon is running, cagents exposes a system-tray item that opens the UI and can stop the daemon. The daemon remains independent of the UI so terminals survive window and UI-process restarts.

Full and browserless production builds share the established protocol-3 daemon socket and session store. A protocol change must deliberately retain or migrate that namespace. One shared application lease prevents their UI processes from racing writes, and startup rejects an older running UI or Debian peer package that predates the lease marker. Upgrade that peer before switching variants. Stop the shared daemon explicitly with either control command (`cagents daemon stop` or `cagents-browserless daemon stop`); installers and package upgrades stop the daemon owned by the package being changed while preserving a compatible peer package's daemon.

Cargo debug builds and builds with the explicit `development` feature use a separate daemon, session store, application lease, and runtime namespace. They do not register a tray item or autostart entry, so source-tree development can run beside the installed production application. Release builds without that feature are production builds.

cagents follows the XDG base-directory convention. Production paths are:

- Settings: `${XDG_CONFIG_HOME:-$HOME/.config}/cagents/settings.toml`
- Session database: `${XDG_DATA_HOME:-$HOME/.local/share}/cagents/sessions.sqlite3`
- Terminal transcripts: `${XDG_DATA_HOME:-$HOME/.local/share}/cagents/transcripts/`
- Cache, when present: `${XDG_CACHE_HOME:-$HOME/.cache}/cagents/`
- Daemon socket and lock: `$XDG_RUNTIME_DIR/cagents/` (shared by full and browserless protocol version 3 builds)

Development substitutes `cagents-development` for the final `cagents` directory component.

Program files installed by `install.sh` live separately under `${XDG_DATA_HOME:-$HOME/.local/share}/cagents-install/`.

### Settings

`settings.toml` uses the version 3 schema below. The original runtime and layout
keys remain required; newer preferences are optional when reading older files
and use the documented defaults. Unknown keys, unsupported schema versions,
malformed TOML, conflicting shortcuts, and values outside the documented ranges
are rejected.

```toml
schema_version = 3
max_hot_cards = 6
hibernation_delay_ms = 30000
terminal_font_size = 14.0
terminal_font_family = ""
sidebar_width = 280.0
inspector_width = 360.0
default_column_fraction = 0.72
terminal_scrollback_lines = 10000
link_open_destination = "integrated_browser"
link_open_gesture = "control_click"

[keyboard_shortcuts]
new_terminal = ["Ctrl+T"]
close_focused_column = ["Ctrl+W"]
search_projects = ["Ctrl+F"]
previous_column = ["Alt+Q", "Alt+Left"]
next_column = ["Alt+W", "Alt+Right"]
```

| Key | Type | Default | Accepted values |
| --- | --- | --- | --- |
| `schema_version` | integer | `3` | exactly `3` |
| `max_hot_cards` | non-negative integer | `6` | `1` through `9223372036854775807` |
| `hibernation_delay_ms` | non-negative integer | `30000` | `0` through `9223372036854775807` milliseconds |
| `terminal_font_size` | float | `14.0` | finite values from `8.0` through `40.0`, inclusive |
| `terminal_font_family` | string | `""` | empty for the system monospace font, or a non-whitespace family name up to 128 bytes. The family must be fixed-pitch; one whose glyph widths differ falls back to the system monospace font, because the grid is a single pitch |
| `terminal_scrollback_lines` | non-negative integer | `10000` | `0` through `1000000` lines for newly started terminals |
| `sidebar_width` | float | `280.0` | finite values from `240.0` through `400.0`, inclusive |
| `inspector_width` | float | `360.0` | finite values from `320.0` through `420.0`, inclusive |
| `default_column_fraction` | float | `0.72` | finite values from `0.25` through `1.0`, inclusive |
| `link_open_destination` | string | `"integrated_browser"` | `"integrated_browser"` or `"system_default"` |
| `link_open_gesture` | string | `"control_click"` | `"control_click"` or `"control_shift_click"` |
| `keyboard_shortcuts.*` | array of strings | shown above | one or more chords, each containing Ctrl, Alt, or Super and one A-Z, 0-9, Left, or Right key; Shift is optional. Every chord in the table must be distinct, and a bound chord is reserved rather than forwarded to the process inside a card |

Version 1 and 2 TOML files are upgraded while loading. Sidebar widths accepted
by version 1 are clamped to the current visible range; other values are
preserved. Version 2 files receive the default inspector width.

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
sudo apt remove cagents             # full package
sudo apt remove cagents-browserless # browserless package
```

APT removal and purge do not delete files in the user's XDG directories.

## Releases

Once a canonical GitHub repository is configured, pushing a `v*` tag runs `.github/workflows/release.yml` on Ubuntu 22.04 with Rust 1.93 and Zig 0.15.2. The tag must exactly equal `v` plus the version in `Cargo.toml`; a mismatch fails before package publication. A successful release runs locked, all-target tests for both feature configurations in the release profile, then packages the full and browserless builds while reusing their release-profile dependency artifacts. The workflow records resolved-package counts, build times, stripped installed sizes, and compressed sizes; validates both sets of desktop and AppStream metadata; stamps the canonical repository into `dist/install.sh`; regenerates checksums; and creates a non-draft GitHub release containing:

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

The package script keeps the existing full package as its default and writes browserless build outputs to a separate Cargo target directory. Run both variants to compare their generated measurement reports without either artifact overwriting the other:

```sh
./scripts/package-linux.sh --variant full
./scripts/package-linux.sh --variant browserless
cat dist/cagents-"${VERSION}"-sizes.txt \
  dist/cagents-browserless-"${VERSION}"-sizes.txt
```

The workflow uses its least-privilege GitHub token for publication through the preinstalled `gh` CLI; it does not require a maintainer's personal `gh` login.

## License

cagents is licensed under the MIT License. See `LICENSE`.
