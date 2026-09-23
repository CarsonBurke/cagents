# User guide

- [Workspace](#workspace)
- [Navigation](#navigation)
- [Inspector and source control](#inspector-and-source-control)
- [Pull request review](#pull-request-review)
- [Worktrees](#worktrees)
- [Windows](#windows)
- [Settings](#settings)
- [Editor](#editor)
- [Native agent chat](#native-agent-chat)
- [Terminals and agent status](#terminals-and-agent-status)
- [Standalone editor (ceditor)](#standalone-editor-ceditor)

Launch **cagents** from the application menu, or run `cagents ui`.

## Workspace

- **Projects** on the left lists repositories and their worktrees. Opening an
  already registered repository selects it without duplicating the project or
  disturbing existing sessions. Selecting a worktree restores its panes;
  selecting it again does not create another terminal.
- Each worktree has a horizontal strip of panes: terminals, native chats,
  editors, repository views, and browsers. A tab strip contains only its
  selected worktree or channel. Some settings and shortcut labels call a pane
  in the strip a **column**, and a few messages call it a **card**; all three
  mean the same thing.
- **New terminal** (`Ctrl+T`) starts in the selected worktree and opens directly
  to the right of the focused pane. The **New tab** menu offers **New
  thread**, **Terminal**, and **Browser** with keyboard navigation; New thread
  opens the model selector.
- **Clone focused pane** (`Alt+E`) opens another pane of the same kind:
  terminals launch a separate process, native chats start a separate
  conversation, and browser and repository panes open at the same location.
  Clones never share the original process or conversation.
- **New channel** (`Alt+G`, or the channel-plus button on a project header)
  starts an independent native conversation in the project's main worktree.
  Channels have their own sidebar rows and panes. A channel holds the chats
  opened while it was selected, and its row shows how many when there is more
  than one.
- Editor tabs use the ceditor mark; recognized agent terminals and native chats
  use their OMP, Claude, or Codex provider mark.
- Drag a project header to reorder projects; the order survives restarts.
  Project headers stay visible while scrolling. The primary worktree stays
  first, and the sort control orders the rest by newest, oldest, or name.
- Worktree labels prefer the matching pull request title and fall back to a
  readable branch basename; tooltips and search keep the full branch name.
  Rows show matching pull requests and their CI status. Unread updates stay
  emphasized until acknowledged, and viewing a waiting agent clears its
  attention badge without changing its activity. The All, Attention, and Pull requests views keep separate
  scroll positions.
- Right-click project or worktree rows for their actions. Icon tooltips appear
  after one second.
- Contextual external-editor icons open the current project, worktree, or file
  in your external editor.

## Navigation

| Action | Default |
| --- | --- |
| New terminal | `Ctrl+T` |
| New channel | `Alt+G` |
| Clone focused pane | `Alt+E` |
| Close focused column | `Ctrl+W` |
| Search projects | `Ctrl+F` |
| Previous / next column | `Alt+Q` / `Alt+W`, or `Alt+Left` / `Alt+Right` |
| Cycle pane width | `Alt+R` |
| Previous / next workspace | `Alt+Shift+Tab` / `Alt+Tab` |
| Mark review file viewed | `Alt+V` |
| Next / previous review file | `Alt+Down` / `Alt+Up` |

Every shortcut is configurable; see
[Configuration](configuration.md#keyboard-shortcuts).

- Hold the column shortcuts to move repeatedly. Navigation scrolls only far
  enough to reveal the target pane.
- **Alt+R** cycles the focused pane through **1/3, 1/2, 2/3, and full**
  viewport width, repeating while held. Widths stay relative to the viewport
  when the window is resized. New panes default to half the viewport.
- Mouse-wheel and trackpad scrolling stay inside pane content; they do not pan
  the workspace strip.
- **Activate panes on hover** is on by default and changes focus without
  scrolling; turn it off in Settings to require a click.
- Switching worktrees remembers each strip's scroll position and focused
  pane. Hidden terminals release their presentation while their PTYs keep
  running, and editor buffers stay intact. Scroll positions, worktree
  selection, and pane focus survive restarts independently in each window.
- Closing the application checks unsaved editors in every worktree, not only
  the visible one. Deleted worktrees disappear from the sidebar without moving
  their panes to another worktree.

## Inspector and source control

The right-hand inspector has icon tabs for **Files**, **Changes**, **Pull
request**, and **Details**; the sidebar toggle hides or reveals it.

- **Files** browses the selected worktree and can list untracked, nonignored
  file names.
- **Changes** provides diffs, staging, commits, push/publish, and history. Its
  views are **Changes** (staged, unstaged, and untracked files), **Branch**
  (everything changed since the merge base with the default branch), and
  **History** (commits, their changed files, and diffs against the first
  parent, or the empty tree for an initial commit). Changes and Branch show
  their file counts.
- **Pull request** inspects the worktree's matching PR, grouping description,
  checks, and discussion in collapsible sections.
- **Details** follows the focused pane.

File lists support **Tree** and **Flat** modes, and filters open from their
search icons. Change rows color their status and line counts and swap them for
action buttons on hover. **Untracked** starts expanded, excludes ignored files,
and is read-only while expanded; expanded untracked text files show measured
additions, while binary, unsafe, oversized, or budget-limited files keep
unknown counts. Visible repository views refresh automatically while the
application is focused.

If pull-request creation fails, cagents restores the submitted title,
description, branches, and draft setting. If the remote outcome is uncertain,
the form stays editable but another attempt is blocked until you check GitHub
and confirm that no pull request was created. A GitHub refresh immediately
revokes mutation authority: missing CLI, unauthenticated, unassociated, loading,
failed, and ready-empty are distinct states. Pull request rows kept from an
earlier refresh are stale display data, never a target for changes, and only a
matching refresh re-enables creation.

## Pull request review

Clicking a pull request row in Projects opens a **pull request pane** for that
repository: the project's open pull requests on the left, and the selected
one's changed files with their GitHub **viewed** marks under a header that shows
progress and links to GitHub.

- Choosing a file shows its diff against the fetched merge base without
  touching the working tree, so staging and the diff-basis switch are absent.
- Marks apply to files or whole directories and are sent to GitHub as you make
  them; a refused mark reverts and says so.
- **Alt+V** marks the open file viewed and moves on; **Alt+Down** and
  **Alt+Up** step through files.
- One pane serves each repository, and a restored pane reopens its pull request
  once GitHub has been read.
- Settings → Links → **Open pull requests in** switches rows back to plain
  links.

## Worktrees

When creating a worktree, enter a branch name, a Linear issue ID such as
`APP-42`, or an `https://linear.app/.../issue/APP-42/title-slug` URL. URLs use
the issue ID and title slug without fetching Linear metadata. New worktrees go
under `worktrees/` in the repository by default; Settings accepts an absolute
or repository-relative parent, and the creation dialog accepts an explicit
destination.

Worktree creation is journaled: a versioned request and an exclusive mutation
identity are recorded before any directory or Git effects, and progress stays
visible through validation, base resolution, Git, and reconciliation.
**Cancel** requests termination once; it does not promise rollback. After a
restart, journal records are rehydrated without replaying commands, and process
leases, Git locks, checkout identity, and completion evidence are checked. Only
a proven **NotCreated** outcome permits a fresh attempt; **OutcomeUnknown**
requires manual inspection. The Git ownership lock is held through the durable
Created commit, and a failure to release it is reported rather than hidden.

## Windows

`cagents ui --new-window` opens another independently movable window, and
`cagents ui --window NAME` opens or activates a named durable window. Plain
`cagents ui` activates the primary window.

New windows copy the source workspace once, then keep their own selection, pane
widths, inspector, and scroll positions. Terminal and native-chat sessions are
owned by the daemon, so input and explicit process closure affect every window
showing that session. Closing a window leaves its processes running.

## Settings

Open **Settings** from the gear in the lower-left corner. Valid changes save
automatically; invalid drafts show an error without replacing the last valid
value. Appearance covers application themes and interface scaling. Terminal
settings cover font family and size, line spacing, padding, cursor shape
(a beam by default), scrollback, and color themes. A thin draggable scrollbar
appears on the focused terminal when it has scrollback. The file format and
every key are documented in [Configuration](configuration.md).

## Editor

- Files save automatically after a pause. Conflicts, save errors, and
  durability warnings stop autosave and stay visible; autosave never silently
  overwrites external changes. Turn off **Save files automatically** in Settings
  to save manually.
- **Wrap long lines** is off by default. Editor and terminal text default to 14
  logical pixels; a terminal font size you already saved is kept.
- Images open as previews. File, view, edit, and diff controls sit in a toolbar
  above the content.
- Working-tree diffs edit the real file buffer with the same selection, IME,
  and safe-save behavior; removed content is selectable read-only context.
  Staged and historical content are read-only. Syntax highlighting is kept in
  file and diff views.
- Comparisons load on demand and cancelled jobs stop. Switching files or modes
  keeps bounded in-memory viewport positions without duplicating buffers or
  writing every scroll to disk.

## Native agent chat

Supported OMP, Claude, and Codex terminal sessions offer **Switch to chat**,
and the New tab menu starts a native conversation without a terminal.

The switch uses the provider's exact conversation identity, never terminal-text
scraping, a most-recent-session lookup, or a fork, and there is no
conversation chooser or ID to copy. A confirmed fresh OMP session can switch
before it has saved a conversation: cagents waits for the replacement's
resumable identity before stopping the source. Missing or ambiguous identities
leave the terminal intact; resume the exact conversation in the provider CLI
and retry. An unrelated, unmanaged terminal without a proven identity is never
touched. Managed Codex threads keep their identity across native and CLI views
automatically.

- **OMP and Claude** use a stop-and-resume handoff: active work is interrupted,
  the CLI stops, and native chat resumes the same conversation. Returning to
  the terminal stops the native writer before resuming the CLI. Unsent drafts,
  attachments, and pending commands block the return so they are not lost.
  This is a handoff, not a simultaneous live view: only one side runs at a
  time, and available history depends on the provider. The original launch
  recipe and terminal descriptor are kept for the return.
- **Codex** threads use a cagents-owned local server, so native and CLI views
  attach to the same thread, even mid-response, and switching keeps drafts,
  attachments, and pending native commands. Existing sessions on a shared Unix
  socket can also be attached; remote WebSocket endpoints are not supported.
  cagents never picks a thread by directory or recency.

The handoff keeps the source process's provider account, environment, and trust
store paths. If credentials or provider settings cannot be transferred safely,
the switch refuses before stopping the terminal; it never substitutes the
daemon's account. GUI-only credentials and authenticated proxy settings travel
through an execution-only channel, not the saved session descriptor, and stay
in daemon memory when you switch to the terminal and back. A full daemon upgrade
refuses before shutdown if its recovery path cannot reproduce the same
execution environment; it never serializes credentials into the upgrade
checkpoint or silently chooses another account. A completed stop-and-resume
handoff keeps a lightweight replay receipt until the terminal replacement is
durably saved, including across restarts; unresolved handoffs block a full
upgrade.

The chat view shows streamed messages, tool activity, and provider approval and
input requests. Controls follow provider capabilities: OMP supports steering,
queued follow-ups, attachments, compaction, and model and thinking selection;
Claude supports attachments, advertised models and slash commands, compaction
when advertised, and permission modes; Codex controls depend on the session's
advertised capabilities, and image attachments need an image-capable model.
Unsupported controls are not offered, and are never imitated with terminal
keystrokes.
Usage and cost come from provider reports.

Chat work keeps running in the background when you switch worktrees or close
the window. **Stop and close chat** interrupts active work and waits for the
daemon before removing the pane. Disconnects keep the draft and offer
**Reconnect to this session**. Transcripts use a private disk-backed cache and
pageable display windows; use **Load earlier history** and **Jump to latest**
to navigate. The cache is not a permanent archive or a fixed disk quota:
explicitly closing the chat releases it, and provider session storage remains
the source for resumed history.

## Terminals and agent status

- Ctrl-click an HTTP or HTTPS address in a terminal to open it using the
  configured link behavior.
- Closing an idle shell needs no confirmation. A running agent, foreground
  child, background job, or uncertain process state asks first, with Close
  focused; Cancel or Escape keeps the pane. The close button and `Ctrl+W`
  follow the same rule. The decision uses the process tree, not prompt
  instrumentation, so a shell builtin with no child process closes without
  asking. **Don't ask me again** turns confirmation off; Settings → **Confirm
  closing active processes** turns it back on.
- Every close, confirmed or not, first rechecks the terminal's identity with
  the daemon. If the daemon is unreachable, the pane is kept.
- Worktree rows distinguish unread updates from **working** and **waiting**
  agents. Attention is tracked per pane: activating a pane acknowledges only
  that pane, not every session in the worktree.
- OMP status comes from its terminal-title lifecycle, Codex from its local
  shared-daemon API, Claude from `agents --json` for the detected CLI and
  account, and native chats from provider protocol events. When evidence is
  unsupported or unavailable, status stays unknown; output volume alone never
  counts as working.
- Status queries are bounded, cached, run outside the terminal input loop, and
  never request snapshots of hidden terminals. Hidden terminals keep running
  without rendering.

## Standalone editor (ceditor)

`ceditor` is a separate native application in the same Cargo workspace. It
shares the editor and repository UI without starting the cagents daemon,
terminal host, or embedded browser.

```sh
scripts/install-ceditor                   # installs to ~/.local; --prefix PATH elsewhere
ceditor /path/to/project
scripts/run-ceditor-development /path/to/project   # isolated development identity
```

- Open tabs, their order, and the active tab restore on launch; explicit paths
  are also opened, and a new session with no paths browses the current
  directory.
- **Ctrl+T** or the plus control creates an untitled note in a scratch folder
  next to the editor settings; **Ctrl+S** then chooses where it lives.
- Its settings, including file-list mode, are stored separately from cagents.
- Right-click a file to rename, duplicate, or copy its absolute or relative
  path. Rename and duplicate never overwrite an existing file. Unsaved changes
  are resolved before switching files, and cancelling a file chooser leaves the
  current document unchanged.
- Language-server assistance is off by default; syntax highlighting, editing,
  saving, and Git work without it. Enable it in Editor settings for
  diagnostics, completion, and symbol information; turning it off shuts the
  servers down. The switch is independent of the `language_servers`
  configuration. Service failures appear in the header's **Notices** tray with
  error details and server stderr; closing the tray keeps them, and **Dismiss
  notices** clears them.
- A Git tab's sidebar switches between **Files**, **Changes**, and **Pull
  requests**. Pull requests come from the GitHub repository named by the tab's
  remotes (`origin` first) through `gh`, with the same viewed marks and
  **Alt+V**, **Alt+Down**, and **Alt+Up** shortcuts, configurable in Editor
  settings.
