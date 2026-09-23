# Configuration

- [Files and directories](#files-and-directories)
- [Settings file](#settings-file)
- [Keyboard shortcuts](#keyboard-shortcuts)
- [Terminal themes](#terminal-themes)
- [Language servers](#language-servers)
- [Default shell](#default-shell)

Most preferences are edited from **Settings** (the gear in the lower-left
corner). Valid changes save automatically; an invalid draft shows an error
without replacing the last valid value.

## Files and directories

cagents follows the XDG base-directory convention. Production paths are:

- Settings: `${XDG_CONFIG_HOME:-$HOME/.config}/cagents/settings.toml`
- Session database: `${XDG_DATA_HOME:-$HOME/.local/share}/cagents/sessions.sqlite3`
- Named-window databases: `${XDG_DATA_HOME:-$HOME/.local/share}/cagents/windows/<name>/sessions.sqlite3`
- Full-upgrade checkpoint: `${XDG_DATA_HOME:-$HOME/.local/share}/cagents/daemon-upgrade/`
- Terminal transcripts: `${XDG_DATA_HOME:-$HOME/.local/share}/cagents/transcripts/`
- Cache, when present: `${XDG_CACHE_HOME:-$HOME/.cache}/cagents/`
- Daemon socket and lock: `$XDG_RUNTIME_DIR/cagents/` (shared by full and
  browserless builds; protocol changes do not create a second owner)

Development builds substitute `cagents-development` for the final `cagents`
directory component. Program files installed by `install.sh` live separately
under `${XDG_DATA_HOME:-$HOME/.local/share}/cagents-install/`.

## Settings file

Preferences live in `${XDG_CONFIG_HOME:-$HOME/.config}/cagents/settings.toml`
(`cagents-development` in place of `cagents` for development builds). The
Settings pane edits the same file, so most people never open it by hand. A few
preferences exist only in the file: language servers and a custom terminal
palette.

The file uses schema version 3. Seven keys are required: `schema_version`,
`max_hot_cards`, `hibernation_delay_ms`, `terminal_font_size`,
`sidebar_width`, `inspector_width`, and `default_column_fraction`. Every other
key is optional and takes the default below when it is missing. Unknown keys,
unsupported schema versions, malformed TOML, conflicting shortcuts, and
out-of-range values are errors. A file that fails to load is reported when the
app starts. That session then runs on defaults and does not write the file, so
your file is not overwritten. To replace a broken file from the app, use
**Reset all settings**.

The file is read at startup. A save writes only the fields that window changed
since its last save. As a result, several windows can share the file, and
edits you make by hand while the app runs are kept, but they take effect only
after a restart.

### Example

This is the file cagents writes with every default, in the order it writes
the keys. `language_servers` is left out here: when it is missing, the built-in
list applies (see [Language servers](#language-servers)).

```toml
schema_version = 3
max_hot_cards = 6
hibernation_delay_ms = 30000
focus_follows_mouse = true
app_theme = "system"
ui_scale = 1.0
terminal_line_height = 1.286
terminal_scroll_speed = 1.0
terminal_padding = 8.0
terminal_scroll_size = 2
terminal_cursor_style = "beam"
terminal_theme = "dark"
terminal_font_size = 14.0
terminal_font_family = ""
sidebar_width = 260.0
inspector_width = 380.0
default_column_fraction = 0.5
terminal_scrollback_lines = 10000
link_open_destination = "integrated_browser"
link_open_gesture = "control_click"
pull_request_open_target = "review"
managed_worktree_directory = "worktrees/"
worktree_sort = "newest"
editor_auto_save = true
editor_wrap_lines = false
editor_syntax_theme = "auto"
confirm_terminal_close = true
chat_enter_sends = true
show_status_bar = true

[keyboard_shortcuts]
new_terminal = ["Ctrl+T"]
new_channel = ["Alt+G"]
clone_pane = ["Alt+E"]
close_focused_column = ["Ctrl+W"]
search_projects = ["Ctrl+F"]
previous_column = ["Alt+Q", "Alt+Left"]
next_column = ["Alt+W", "Alt+Right"]
cycle_pane_width = ["Alt+R"]
previous_workspace = ["Alt+Shift+Tab"]
next_workspace = ["Alt+Tab"]
review_toggle_viewed = ["Alt+V"]
review_next_file = ["Alt+Down"]
review_previous_file = ["Alt+Up"]
```

TOML does not allow a top-level key after a table header. Put every plain
`key = value` line, including `language_servers = []`, before
`[keyboard_shortcuts]` or any other `[table]` or `[[array]]` header.

### Keys

Floating-point ranges include both ends. NaN and infinity are rejected.

| Key | Type | Default | Accepted values |
| --- | --- | --- | --- |
| `schema_version` | integer | `3` | Exactly `3`. Required |
| `max_hot_cards` | integer | `6` | `1` through `9223372036854775807`. The number of columns kept live; extra background columns can hibernate. Required |
| `hibernation_delay_ms` | integer | `30000` | `0` through `9223372036854775807` milliseconds before a background column hibernates. Required |
| `focus_follows_mouse` | boolean | `true` | Activates the pane under the pointer without scrolling |
| `app_theme` | string | `"system"` | `"system"`, `"dark"`, `"light"`, `"catppuccin_mocha"`, `"catppuccin_latte"`, `"dracula"`, `"nord"`, `"tokyo_night"`, `"gruvbox_dark"`, or `"solarized_light"`. Colors the application; terminal and syntax colors are set separately |
| `ui_scale` | float | `1.0` | `0.8` through `1.5`. Multiplies display scaling |
| `terminal_line_height` | float | `1.286` | `1.0` through `2.0`, relative to the font size |
| `terminal_scroll_speed` | float | `1.0` | `0.25` through `4.0`. Multiplies wheel and trackpad scrolling. Mouse wheel events sent to applications that track the mouse are not scaled |
| `terminal_padding` | float | `8.0` | `0.0` through `24.0` logical pixels |
| `terminal_scroll_size` | integer | `2` | `1` through `10` lines per mouse-wheel notch. Trackpad scrolling stays proportional |
| `terminal_cursor_style` | string | `"beam"` | `"application"` uses the cursor shape the program requests. `"block"`, `"beam"`, and `"underline"` override that request |
| `terminal_theme` | string or table | `"dark"` | One of the 25 terminal presets (see [Terminal themes](#terminal-themes)), independent of `app_theme`. When **Custom** is selected, the file stores the palette here as a `[terminal_theme.custom]` table |
| `terminal_font_size` | float | `14.0` | `8.0` through `40.0` logical pixels. Required |
| `terminal_font_family` | string | `""` | Empty for the system monospace font, or a family name of up to 128 bytes that is not only whitespace. The terminal grid needs one character width. A family whose glyph widths differ falls back to the system monospace font |
| `sidebar_width` | float | `260.0` | `220.0` through `420.0`. Required |
| `inspector_width` | float | `380.0` | `320.0` through `480.0`. Required |
| `default_column_fraction` | float | `0.5` | `0.25` through `1.0`. The share of the workspace a new column takes. Required |
| `terminal_scrollback_lines` | integer | `10000` | `0` through `1000000` lines. Applies to terminals started after the change |
| `link_open_destination` | string | `"integrated_browser"` | Where an ordinary link click opens: `"integrated_browser"` or `"system_default"` |
| `link_open_gesture` | string | `"control_click"` | The gesture that always opens the system browser: `"control_click"` or `"control_shift_click"`. The other gesture always opens the in-app browser |
| `pull_request_open_target` | string | `"review"` | What opening a pull request in the sidebar does: `"review"` opens the in-app review pane, and `"link"` opens the pull request's web page as a link, using the settings above |
| `managed_worktree_directory` | string | `"worktrees/"` | A path that is not blank and has no control characters. It can be absolute or relative to the repository root |
| `worktree_sort` | string | `"newest"` | `"newest"`, `"oldest"`, or `"alphabetical"`. The main worktree always comes first. Imported worktrees with no known creation date come after dated ones, sorted by path |
| `editor_auto_save` | boolean | `true` | Saves edited files after a pause. Auto-save stops when there is a conflict or a save error |
| `editor_wrap_lines` | boolean | `false` | Wraps long lines in the native editor. When it is off, you scroll sideways |
| `editor_syntax_theme` | string | `"auto"` | Syntax colors for editors and diffs: `"auto"` (follows the app theme), `"solarized_dark"`, `"mocha"`, `"ocean"`, `"eighties"`, or `"inspired_git_hub"` |
| `confirm_terminal_close` | boolean | `true` | Asks before closing a terminal that has a foreground program or background jobs. An idle shell closes without asking |
| `chat_enter_sends` | boolean | `true` | When on, Enter sends a chat message and Shift+Enter starts a new line. When off, Enter starts a new line and Ctrl+Enter sends |
| `show_status_bar` | boolean | `true` | Shows the bottom status bar with the usage limits left on each signed-in provider account. Hiding it also stops the usage checks that feed it, unless the Accounts pane is open |
| `language_servers` | array of tables | built-in list | See [Language servers](#language-servers). An empty list turns off language servers |
| `keyboard_shortcuts.*` | array of strings | shown above | See [Keyboard shortcuts](#keyboard-shortcuts) |

### Keyboard shortcuts

Each action takes a list of chords. A chord has one key and one or more
modifiers, joined by `+`. Names are case-insensitive.

- Modifiers: `Ctrl` (or `Control`), `Alt`, `Super` (or `Meta`), and `Shift`.
  Each chord needs `Ctrl`, `Alt`, or `Super`. `Shift` is optional.
- Keys: `A`-`Z`, `0`-`9`, `Left`, `Right`, `Up`, `Down`, or `Tab`.
- No chord can appear twice, even under different actions.
- A chord you bind here is not passed to the program running in a card.
  For that reason, bare `Ctrl+C`, `Ctrl+D`, `Ctrl+Z`, `Ctrl+S`, and `Ctrl+Q`
  cannot be bound. Versions with more modifiers, such as `Ctrl+Shift+C`, can
  be bound.
- `new_terminal`, `close_focused_column`, `search_projects`,
  `previous_column`, and `next_column` need at least one chord. The rest
  (`new_channel`, `clone_pane`, `cycle_pane_width`, the two workspace actions,
  and the three `review_*` actions) can be empty lists (`[]`), which leaves
  them unbound.

### Terminal themes

There are 25 dark and light terminal presets. The values are `"dark"`,
`"light"`, `"solarized_dark"`, `"solarized_light"`, `"dracula"`,
`"catppuccin_mocha"`, `"catppuccin_macchiato"`, `"catppuccin_frappe"`,
`"catppuccin_latte"`, `"nord"`, `"gruvbox_dark"`, `"gruvbox_light"`,
`"tokyo_night"`, `"tokyo_night_storm"`, `"tokyo_night_day"`, `"rose_pine"`,
`"rose_pine_moon"`, `"rose_pine_dawn"`, `"kanagawa_wave"`, `"kanagawa_lotus"`,
`"one_dark"`, `"github_dark"`, `"github_light"`, `"everforest_dark"`, and
`"everforest_light"`. Choose one under **Settings → Terminal → Color theme**.

To make your own palette:

1. Add a `[custom_terminal_theme]` table after the top-level keys.
2. Restart the app.
3. Choose **Custom** in the terminal color theme dropdown.

cagents keeps this table when you switch to a preset. On load, this table
replaces any palette stored under `terminal_theme`, so editing the table
updates an active custom theme.

The table rules:

- Colors use `#RRGGBB`.
- `ansi` has exactly 16 colors: normal colors 0-7, then bright colors 8-15.
- `light` is optional and defaults to `false`. It tells terminal programs
  whether the palette is light or dark.
- Colors that programs set themselves, with truecolor or OSC palette changes,
  take precedence over this palette.

```toml
[custom_terminal_theme]
foreground = "#d8d8d8"
background = "#101010"
cursor = "#ffffff"
light = false
ansi = [
  "#3b4252", "#bf616a", "#a3be8c", "#ebcb8b",
  "#81a1c1", "#b48ead", "#88c0d0", "#e5e9f0",
  "#4c566a", "#d08770", "#b1d196", "#f0d399",
  "#8fbcdb", "#c5a0c0", "#93ccdc", "#eceff4",
]
```

### Language servers

`language_servers` lists external Language Server Protocol (LSP) servers for
the native editor. If the key is missing, cagents uses this built-in list. If
the key is present, its list replaces the built-in one entirely; the two are
not merged. `language_servers = []` turns off language servers.

| id | Command | Extensions |
| --- | --- | --- |
| `rust-analyzer` | `rust-analyzer` | `rs` |
| `pyright` | `pyright-langserver --stdio` | `py` |
| `typescript-language-server` | `typescript-language-server --stdio` | `js`, `mjs`, `cjs`, `jsx`, `ts`, `mts`, `cts`, `tsx` |
| `gopls` | `gopls` | `go` |
| `clangd` | `clangd` | `c`, `h`, `cc`, `cpp`, `cxx`, `hh`, `hpp`, `hxx` |
| `lua-language-server` | `lua-language-server` | `lua` |
| `bash-language-server` | `bash-language-server start` | `sh`, `bash` |

A server entry has these fields:

- `id`: a unique name that is not empty.
- `command`: an argument list, not a shell command line. Its first item names
  the executable. A plain name is looked up on `PATH`. A name that contains `/`
  is treated as a path.
- `languages`: one or more tables. Each has an LSP `language_id` and at least
  one `extensions` value. Write extensions in lowercase without the leading
  dot. When cagents matches a file, the case of the file's extension does not
  matter.
- `root_markers` (optional): relative paths that mark a project root, such as
  `Cargo.toml`. They cannot contain empty, `.`, or `..` path components. To
  find a file's project root, cagents checks the file's folder and then each
  parent folder, and stops at the worktree root. The first folder that contains
  a marker is the project root. If no folder has a marker, the worktree root is
  used.

An extension can belong to only one server.

```toml
[[language_servers]]
id = "rust-analyzer"
command = ["rust-analyzer"]
root_markers = ["Cargo.toml", "rust-project.json", ".git"]

[[language_servers.languages]]
language_id = "rust"
extensions = ["rs"]
```

`[[language_servers]]` headers can appear before or after
`[keyboard_shortcuts]`. The inline `language_servers = []` must come before
every table header.

### Upgrading older files

cagents upgrades version 1 and version 2 files when it loads them. It writes
them back as version 3 the next time it saves.

- **Version 1:** a `sidebar_width` from 180 through 480 is clamped to the
  current range of 220 through 420. Other values are kept as written.
- **Version 2:** these files get the default `inspector_width` and the built-in
  language servers. A shortcut written as a single string becomes a list with
  one chord.
- **Old scroll key:** early version 3 files set lines per notch with an integer
  `terminal_scroll_speed`. If `terminal_scroll_size` is missing, that value is
  moved to `terminal_scroll_size`, and an old value of `1` becomes the new
  default of `2`.
- **Renamed shortcut:** a `new_thread` shortcut, the name used before channels
  were named, is read as `new_channel`. If both names are present,
  `new_channel` is used.
- **Newer shortcuts:** a `[keyboard_shortcuts]` table that lacks a newer action
  gets its default chord. These actions are `new_channel` (Alt+G), `clone_pane`
  (Alt+E), `cycle_pane_width` (Alt+R), `previous_workspace` (Alt+Shift+Tab),
  `next_workspace` (Alt+Tab), `review_toggle_viewed` (Alt+V),
  `review_next_file` (Alt+Down), and `review_previous_file` (Alt+Up). If
  another action already uses that chord, the existing binding is kept and the
  new action stays unbound (`[]`) until you give it a free chord.
- **Your bindings:** any binding you set yourself is kept, including an empty
  list. A conflicting binding that you set yourself is still an error.

In the Settings pane, text fields save valid edits after a short pause.
Leaving or closing the pane also applies valid pending edits. Invalid fields
and disk-write failures stay on screen as errors. Your existing preferences,
such as an `"application"` cursor, are kept and are not replaced by newer
defaults.

### Migrating from settings.json

cagents migrates a `settings.json` in the same directory only when
`settings.toml` does not exist. The JSON file must use schema version 1. Every
field is optional, and unknown fields are rejected. cagents checks the JSON,
writes and syncs the TOML file, and then tries to delete the JSON file. If
deletion fails, the JSON file stays and is ignored. An existing TOML file
always takes precedence. If the TOML file is malformed, cagents reports the
error and does not fall back to the JSON file.

### File safety

- cagents sets the configuration directory to mode `0700` and the settings file
  to mode `0600`.
- Both must be owned by the effective user.
- If any component of the path is a symbolic link, cagents rejects the path.
- Saves write a temporary file and rename it into place.
- A `settings.toml.lock` file keeps separate windows from saving at the same
  time.

### Default shell

New terminal cards use a default shell. So does `cagents terminal spawn` when
you give it no command. cagents tries these in order and uses the first one
that is an absolute path to an executable file:

1. The shell in the effective user's passwd entry.
2. `SHELL`.
3. `/bin/sh`.

This lookup applies only when cagents creates a new default terminal.
Explicit CLI commands and launch recipes restored from saved sessions run
unchanged.
