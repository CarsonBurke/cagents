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

- **Projects** on the left lists folders, repositories, and their worktrees. Opening an
  already registered repository selects it without duplicating the project or
  disturbing existing sessions. Selecting a worktree restores its panes;
  selecting it again does not create another terminal.
- **Open folder** also accepts folders without Git. Select the project row to
  work there with terminals, chats, channels, and the editor. The Files panel
  lists children as you expand directories; Git actions appear only for Git
  projects. Folder projects survive restarts. If you later run `git init`,
  remove and reopen the project to enable Git features; existing open folder
  projects keep their identity and channels. `cagents project open PATH` and
  `cagents channel spawn --repository PATH` accept the same folders.
- Each worktree has a horizontal strip of panes: terminals, native chats,
  editors, repository views, and browsers. A tab strip contains only its
  selected worktree or channel. Some settings and shortcut labels call a pane
  in the strip a **column**, and a few messages call it a **card**; all three
  mean the same thing.
- **New terminal** (`Ctrl+Shift+T`) starts in the selected worktree and opens directly
  to the right of the focused pane. The **New tab** menu offers **New
  thread**, **Terminal**, and **Browser** with keyboard navigation; New thread
  opens the model selector.
- **Clone focused pane** (`Ctrl+Shift+D`) opens another pane of the same kind:
  terminals launch a separate process, native chats start a separate
  conversation, and browser and repository panes open at the same location.
  Clones never share the original process or conversation.
- **New channel** (`Ctrl+Shift+G`, or the channel-plus button on a project header)
  starts an independent native conversation in the project's main worktree.
  Channels have their own sidebar rows and panes. A channel holds the chats
  opened while it was selected, and its row shows how many when there is more
  than one.
- Editor tabs use the ceditor mark; recognized agent terminals and native chats
  use their OMP, Claude, or Codex provider mark.
- Drag a tab title to reorder panes, or hold **Alt** and drag a pane. The
  insertion line previews its new position; releasing the mouse saves the
  order without changing the active pane. Scroll the wheel while dragging to
  reach offscreen panes, or press **Escape** to cancel.
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
| New terminal | `Ctrl+Shift+T` |
| New channel | `Ctrl+Shift+G` |
| Clone focused pane | `Ctrl+Shift+D` |
| Close focused column | `Ctrl+Shift+W` |
| Search projects | `Ctrl+F` (outside terminals) |
| Previous / next column | `Ctrl+PageUp` / `Ctrl+PageDown` |
| Cycle pane width | `Ctrl+Shift+R` |
| Previous / next workspace | `Alt+Shift+Tab` / `Alt+Tab` |
| Mark review file viewed | `Alt+V` |
| Next / previous review file | `Alt+Down` / `Alt+Up` |
| Next unviewed review file | `Alt+Shift+Down` |
| Next / previous changed hunk in a review | `Alt+J` / `Alt+K` |

Every shortcut is configurable; see
[Configuration](configuration.md#keyboard-shortcuts).

- The defaults leave the keys shells, editors, and agent CLIs use to the
  focused terminal, the way GNOME Terminal, Ghostty, and Kitty do: `Ctrl+W`
  deletes a word, `Ctrl+T` and `Alt` letters reach the program. A chord you
  bind is withheld from the terminal, except **Search projects**, which a
  focused terminal receives as input (in an editor it finds in the file), and
  the review chords, which act only in a pull request pane. As in those
  terminals, `Ctrl+PageUp`/`Ctrl+PageDown` and `Ctrl+Shift` letters then no
  longer reach terminal programs such as Vim.
- Earlier versions used `Ctrl+T`, `Ctrl+W`, `Alt+Q`/`Alt+W`,
  `Alt+Left`/`Alt+Right`, `Alt+E`, `Alt+G`, and `Alt+R`. Shortcuts still on
  those defaults move to the new ones; any shortcut you changed is kept. See
  [Upgrading older settings files](configuration.md#upgrading-older-settings-files).

- Hold the column shortcuts to move repeatedly. Navigation scrolls only far
  enough to reveal the target pane.
- **Ctrl+Shift+R** cycles the focused pane through **1/3, 1/2, 2/3, and full**
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

Sidebar branch names and tracked-change counts follow local filesystem events,
independently of GitHub's refresh schedule. Changes to one worktree refresh that
worktree; shared Git refs refresh the project's worktrees. If local watching is
unavailable, cagents shows a notice and checks once a minute until watching
recovers. Network filesystems and submodules retain this fallback because their
changes may not produce local filesystem events.

If pull-request creation fails, cagents restores the submitted title,
description, branches, and draft setting. If the remote outcome is uncertain,
the form stays editable but another attempt is blocked until you check GitHub
and confirm that no pull request was created. A GitHub refresh immediately
revokes mutation authority: missing CLI, unauthenticated, unassociated, loading,
failed, and ready-empty are distinct states. Pull request rows kept from an
earlier refresh are stale display data, never a target for changes, and only a
matching refresh re-enables creation.

## Pull request review

In Projects' **Pull requests** filter, click a project name to browse all of its
open PRs, including those with no local worktree. A matching worktree row opens
its PR directly. The **pull request pane** shows open pull requests on the left, and the selected
one's changed files with their GitHub **viewed** marks under a header that shows
progress and links to GitHub.

The shared browser in cagents and ceditor loads 50 PRs at a time. **Load more
pull requests** follows the next cursor with no fixed total limit. Search by
title, number, or author and filter drafts or check status across the loaded
rows; the counts make that scope visible. An open, focused review refreshes
every 30 seconds, keeping its file marks and discussion current.
The same refresh updates its title, checks, review decision, and base branch.

**Needs your review** means GitHub currently requests the authenticated viewer's
review. **Re-requested** additionally requires an earlier request for the same
reviewer identity in GitHub's timeline; a previous comment or a branch's required
review policy does not count. If history cannot be read, the current request
still appears without a re-requested label. Background discovery continues in
bounded pages, so these filters eventually include requests beyond the browser's
first 50 rows. Changing the authenticated viewer clears the prior viewer's
request state. Sidebar worktree rows show **Review** or **Review again** for the
same requests.

Large reviews mount only visible file rows and their folder ancestors. Viewed
marks load in pages as you scroll or navigate; unfetched marks stay visibly
unknown and cannot be toggled. **Load more viewed states** also fetches a page
on demand. A failed page can be retried without losing the open diff.

Previously opened reviews retain their file list, viewed marks, and selected
file locally. If GitHub is unavailable, enter `#123` in the browser and choose
**Open #123** to read its cached diff. The header identifies the cached viewer;
viewed changes and review submission stay disabled until a fresh GitHub load
confirms the current account and revision. Unconfirmed viewed changes are
reconciled on refresh. The cache retains up to 32 recent reviews within a
32 MiB payload budget; it does not store pending comment bodies or credentials.

- Choosing a file shows its diff against the fetched merge base without
  touching the working tree, so staging and the diff-basis switch are absent.
- Marks apply to files or whole directories and are sent to GitHub as you make
  them; a refused mark reverts and says so.
- **Alt+V** marks the open file viewed and moves on; **Alt+Down** and
  **Alt+Up** step through files.
- The file toolbar switches between tree and flat lists, hides viewed files,
  and jumps to the next unviewed file. Navigation reveals collapsed ancestors
  and skips hidden viewed files. Each file shows its comment and unresolved
  discussion counts; the viewed progress stays visible above the list.
- One pane serves each repository, and a restored pane reopens its pull request
  once GitHub has been read.
- Settings → Links → **Open pull requests in** switches rows back to plain
  links.

Click a diff line number to write a comment. Shift-click another line on the
same side to select an inclusive range. **Save pending comment** keeps the
comment in your GitHub draft review; **Preview** renders its Markdown before
saving. Existing discussions appear below their lines, with outdated threads
above the file. Resolved threads collapse. You can reply, resolve or reopen a
thread, and edit or delete your own pending comments.
Threads whose lines are unavailable, including non-text files, also appear
above the file so their replies and resolution controls remain reachable.

The review bar shows the pending comment count and offers **Comment**,
**Approve**, or **Request changes**, with an optional summary. Save or cancel
an open composer first. **Review and submit** shows the repository, pull
request, exact commit, event, summary, and pending count before submission.
The app rechecks the repository, signed-in account, commit, and pending comment
contents before sending. If a request's answer is lost, it asks you to check
GitHub rather than silently posting it again. The same controls are available
in ceditor's Pull requests panel.

Use **Create worktree from reviewed commit** in the PR header to work on that
exact revision. The suggested branch includes the PR number and commit prefix;
choose a new branch and destination before confirming. A later force-push does
not change the checkout. Cagents uses its regular worktree creation flow;
Ceditor previews the resolved destination, creates the worktree, and offers
**Open worktree**. Both preserve the original checkout and record interrupted
creation for reconciliation instead of silently repeating it.
Ceditor's **Worktrees** footer opens recent operation results, including a
completed worktree recovered after a restart, with an **Open worktree** action.

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

## Scheduled automations

Open **Automations** from the clock button in the sidebar footer. Choose an open
project or worktree, provider, account, prompt, model, and permission mode. Pick
an hourly, daily, weekday, weekly, or five-field cron schedule with an explicit
IANA timezone. New definitions are paused. **Enable…** and **Run now…** show the
exact saved launch settings before confirmation; changing those settings or the
schedule pauses future runs until you confirm again. Renaming preserves the
existing execution settings.

The daemon runs enabled schedules with every window closed. It must itself be
running: cagents does not install a system scheduler. On startup it considers
only the latest missed occurrence within 15 minutes and records older missed
deadlines as skipped. Nonexistent local times during a clock change are skipped;
repeated local minutes run once. Each automation has one active run, and an
overlapping deadline is skipped rather than queued behind it.

An optional shell precheck runs in the chosen directory with a bounded timeout.
Exit zero permits the prompt; another exit skips it. Cancellation and timeout
terminate and reap the precheck's descendants before releasing capacity. An
uncertain result after daemon loss is recorded as interrupted and never resent
automatically. Precheck supervision uses two short-lived helper processes per
active precheck, with at most four prechecks at once.

A fresh conversation closes the automation's idle provider after its result is
durable. **Reuse this automation’s idle agent** keeps its own verified idle conversation for the
next occurrence. If you take over the conversation, automatic cleanup and reuse
stop until ownership is resolved. Removing the chosen account or replacing the
target directory cannot silently redirect a future run to another account or
directory. Closing a project pauses its schedules.

**All run history** includes runs from removed definitions. It pages through
their original names, times, outcomes, and bounded results. Opening history does
not start or attach a provider. **Open agent** explicitly opens a still-live
session; completed fresh sessions remain readable in history. Completion alerts
open this exact run history and survive provider cleanup. Removing a definition
prevents future runs and retains its past runs.

The same operations are available without a window:

```sh
cagents automation create --definition automation.json
cagents automation show AUTOMATION_ID
cagents automation enable AUTOMATION_ID --revision REVISION --confirm
cagents automation run-now AUTOMATION_ID --execution-revision EXECUTION_REVISION --request-id REQUEST_UUID --confirm
cagents automation history
cagents automation history AUTOMATION_ID --limit 50
cagents automation run FIRING_ID
```

Read both revisions from `show`. Reuse the same request UUID when retrying a
manual request whose response was lost; a new UUID means a new run. History
returns `next` with the paired `--before-ms` and `--before-id` cursor for the next
page. `--json` returns the same typed results for scripts. A definition file has
this shape (the repository must already be open):

```json
{
  "name": "Daily review",
  "prompt": "Review the changes and summarize risks.",
  "agent": {
    "provider": "Claude",
    "account": null,
    "target": {
      "repository": "/absolute/project",
      "directory": "/absolute/project",
      "channel": null
    },
    "surface": "Chat",
    "model": null,
    "permission_mode": null
  },
  "schedule": {
    "pattern": { "daily": { "hour": 9, "minute": 0 } },
    "timezone": "America/Vancouver"
  },
  "session_mode": "fresh",
  "precheck": null
}
```

An omitted account selection resolves once when the definition is saved and is
then pinned in the saved definition. Review that account before enabling. Agent
child capabilities cannot create or modify unattended schedules; use an
explicit user CLI session for automation management.

## Settings

Open **Settings** from the gear in the lower-left corner. Valid changes save
automatically; invalid drafts show an error without replacing the last valid
value. Appearance covers application themes and interface scaling. Terminal
settings cover font family and size, line spacing, padding, cursor shape
(a beam by default), scrollback, and color themes. A thin draggable scrollbar
appears on the focused terminal when it has scrollback. The file format and
every key are documented in [Configuration](configuration.md).

The Accounts pane also explains which settings new provider homes share or copy.
See [Account configuration](account-configuration.md) for Claude settings and MCP
copies, Codex's independent configuration option, and keyring isolation.

Removing a managed account prevents new launches and probes from recreating its
home. Existing processes remain attachable. If a saved chat needs that removed
account, its error offers other accounts from the same provider, including the
available system/default account. Choosing one tries to resume the exact saved
conversation; its account and runtime identity change only after the provider
confirms that conversation. A failed or mismatched resume keeps the original
saved descriptor. This cannot recover conversation files deleted with the old
home if the provider cannot access them from the selected account.

Account loading also removes unregistered managed homes older than 24 hours.
Pending sign-ins, registered accounts, homes referenced by linked accounts, and
arbitrary external directories are preserved. Missing, unreadable, or newer
account registries prevent this orphan cleanup.

**Notifications** controls background agent completion, failure or cancellation,
and requests for attention. Alerts are suppressed while their pane is focused
unless **Notify for the focused pane** is enabled. They name the provider and
folder, without copying prompts or output. **Send test notification** reports
whether the desktop portal accepted the request; desktop settings control its
presentation and sound.

The daemon delivers alerts even with every window closed. Clicking one opens its
owning window and selects the session. Native chats use explicit provider turn
outcomes. Terminal Codex sessions use main CLI rollout completion/cancellation
records; Claude sessions use their exact main transcript's `end_turn` and
PID-scoped waiting state. Unsupported providers or log formats remain activity
indicators; idle alone never means success. Initial history is not announced.

Attention and recent event identities are retained in a bounded private journal.
Reconnects do not create one alert per window or replay historical completions.
When the desktop is unavailable, attention remains visible on reopening. Events
from the current daemon retry with bounded backoff, or when a window reconnects;
restored historical records are never reannounced. A process
crash between receipt, journal write and desktop acknowledgement can still lose
or repeat an alert: desktop delivery and local persistence are not one transaction.

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

### Account credential isolation

Choosing a managed or linked Claude/Codex account uses that home's stored login.
Cagents removes inherited credential overrides from its account probes, sign-in
commands, new chats and terminals, and preserves those removals on resume and
daemon upgrade. **System default** keeps the environment behavior of the provider
CLI. Only variable names are saved; cagents does not inspect credential values.
Older saved recipes that name a managed home inside this installation's account
directory receive the same protection when a child is restarted. Older recipes
for linked directories need an explicit account selection to record the policy;
an arbitrary external directory cannot safely be inferred to be a managed login.

For Claude, the removed variables are `ANTHROPIC_API_KEY`,
`ANTHROPIC_AUTH_TOKEN`, `CLAUDE_CODE_OAUTH_TOKEN`,
`CLAUDE_CODE_OAUTH_REFRESH_TOKEN`, `CLAUDE_CODE_OAUTH_SCOPES`,
`CLAUDE_CODE_USE_ANTHROPIC_AWS`, `CLAUDE_CODE_USE_BEDROCK`, `CLAUDE_CODE_USE_MANTLE`,
`CLAUDE_CODE_USE_VERTEX`, `CLAUDE_CODE_USE_FOUNDRY`, `ANTHROPIC_PROFILE`,
`ANTHROPIC_FEDERATION_RULE_ID`, and `ANTHROPIC_ORGANIZATION_ID`. These select
credentials ahead of a stored `/login` according to Claude's
[authentication precedence](https://code.claude.com/docs/en/iam#authentication-precedence)
and [environment reference](https://code.claude.com/docs/en/env-vars).
The refresh-token pair also bypasses browser account selection during sign-in.

For Codex, the removed variables are `OPENAI_API_KEY`, `CODEX_API_KEY`,
`CODEX_ACCESS_TOKEN`, `OPENAI_FEDERATION_RULE_ID`, `OPENAI_IDENTITY_TOKEN_FILE`,
and `OPENAI_WORKLOAD_IDENTITY_CONTEXT`. Codex's
[authentication implementation](https://github.com/openai/codex/blob/7b7d934408e6df20950a7a2501a669650279ab06/codex-rs/login/src/auth/manager.rs)
checks supported process tokens before persistent credentials; its
[workload identity implementation](https://github.com/openai/codex/blob/7b7d934408e6df20950a7a2501a669650279ab06/codex-rs/login/src/auth/workload_identity.rs)
selects process authentication using the federation markers. `OPENAI_API_KEY`
is also removed for provider/version compatibility.

**System default** is the home the provider CLI uses outside cagents:
`CLAUDE_CONFIG_DIR` or `CODEX_HOME` as cagents was started with, otherwise
`~/.claude` or `~/.codex`. A home cagents itself selected is never taken for it.
Every cagents terminal and conversation exports its account's home, and one on
a managed or linked account also lists that home in `CAGENTS_ACCOUNT_HOMES`. A
cagents window, daemon, or `cagents` command started from one - by you or by an
agent - ignores a variable that still holds the listed home, or any path inside
a cagents `accounts/<provider>/<id>` directory, when deciding the system
default. A daemon started before this protection keeps its environment until it
restarts.

The Accounts pane flags a row whose home is not a login of its own: **Login
changed** when a managed or linked home reports a different login than the one
it was added with (the recorded login is kept; sign that account in again to
accept the new one, or remove it), and **Same home** or **Same login** when two
rows are the same directory or signed in to the same login.

Proxy, certificate, and generic cloud variables remain available to the agent's
tools. Provider settings files, credential helpers, and administrator policies
remain user-controlled. This policy does not rewrite those settings or an
already-running conversation. If a terminal later exports an excluded variable,
switching it to native chat refuses before stopping the CLI. Recovery retains
the recorded account selection; choose **System default** to inherit credentials.

### Switching between terminal and chat

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

- Drag over terminal output to select text, then press `Ctrl+Shift+C` to copy.
  Agent terminals keep ordinary clicks available to the CLI while drags select
  text. Hold Alt to send a drag to the agent, or Shift to select text in other
  mouse-aware terminal applications. Empty rows and right-side padding do not
  start selections.
- Ctrl-click an HTTP or HTTPS address in a terminal to open it using the
  configured link behavior.
- Ctrl-click a local `file://` address in a terminal, or follow one in chat,
  to open an editor beside the current pane. A visible clean file editor is
  reused; unsaved buffers stay open. HTML opens as source; **Preview HTML**
  opens the saved file in the integrated browser, or the system browser in
  browserless builds. The selected file is restored with the workspace.
- Closing an idle shell needs no confirmation. A running agent, foreground
  child, background job, or uncertain process state asks first, with Close
  focused; Cancel or Escape keeps the pane. The close button and
  `Ctrl+Shift+W` follow the same rule. The decision uses the process tree, not prompt
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
- Drag a tab title to change its saved order. Scroll the wheel while dragging
  to reach offscreen tabs; **Escape** cancels the drag.
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
