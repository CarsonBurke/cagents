# Worktree CLI

`cagents worktree create/list/remove` uses the selected device's daemon. Run
commands inside a project, or select an open project with `--project ID_OR_PATH` (an
unambiguous open project name is also accepted).

```sh
cagents worktree create feat-x --base main
cagents worktree create fix-ticket --linear ENG-123 --no-setup
cagents worktree create investigate --spawn-agent codex --model gpt-6-luna --reasoning-effort low --prompt 'Inspect the failing test'
cagents worktree list --json
cagents worktree remove feat-x --delete-branch
```

Creation uses the same location setting, local ref resolver and durable Git
journal as the dialog. The daemon notifies open sidebars immediately after a
mutation. Existing branches keep their history; specify `--base` for a new branch.
`--linear` records the worktree-local ticket link during durable settlement.

Project setup uses `.cagents/worktree.toml` and the same exact-configuration trust
as the dialog; see [Worktree setup](worktree-setup.md). Untrusted commands are
refused before creation. Review and trust them in the dialog, or pass `--no-setup`
to skip both copies and commands. Setup output runs in a **Worktree setup** terminal
card scoped to the new worktree. Its exact plan, skip decision, copy progress and
run identity are retained with the creation journal. Saved setup recipes are
never automatically replayed after interruption; use explicit reviewed recovery
in the application. Before retrying, close the previous setup terminal and any
remaining child processes, then review the current configuration again.
A preparation or terminal failure keeps the
created checkout and is reported in the result. `--spawn-agent` waits up to two
minutes for setup to succeed; if setup fails or is still running, its error names
the retained worktree and the agent is not started.

Removal refuses the primary checkout, locked worktrees and worktrees containing
live sessions. Close their sessions before removal. Window cards, editor scopes
and setup preparation also retain directory protection; close those scopes
before using the CLI to remove their checkout. `--force` preserves these active
protection fences. It freshly checks modified,
untracked, notable ignored, unpushed and detached-only work. `--force` explicitly
confirms discarding those files and commits; a detached-only head is first
preserved on a rescue branch. `--delete-branch` runs Git's safe `branch -d` after
removal. It never force-deletes an unmerged branch; if Git refuses, the result
explains that the worktree was removed and its branch retained.

Setup recovery also requires a durable disposition for the previous run. A
missing holder or a free setup lock does not prove its child processes ended.
If the holder was lost before it recorded complete descendant cleanup, Resume
and Skip keep the checkout interrupted and preserve the current intent. Resolve
the retained terminal ownership before trying again. Some early startup errors
can leave this outcome unknown even when no child was observed; they do not
silently grant permission to repeat a shell command.

## JSON version 1

Every result uses the ordinary CLI `{ "ok", "command", "data" }` envelope.
Failure uses the ordinary error envelope and a non-zero exit code. The `data`
object has `schema_version: 1`:

- `worktree.create`: `worktree`, `warnings` (strings), `setup` (null or
  `{ command, run_id, terminal, error }`), and `agent` when requested and accepted.
- `worktree.list`: `worktrees` (array of entries).
- `worktree.remove`: `path`, `branch_deleted` (null when retained), `warnings` (strings), and
  `branch_error` (null unless the requested safe branch deletion failed).
  A branch error returns non-zero with these committed removal details.

A worktree entry contains `path`, `branch` (short name or null), `head_oid`,
`primary`, `detached`, `locked`, `dirty`, `ahead`, `behind`, and `live_sessions`
(array of terminal or chat IDs). `ahead`/`behind` are null when there is no known
upstream comparison; null does not mean zero. Paths belong to the selected device.
Human-readable list output is an aligned table with path, branch, clean/dirty,
ahead, behind and live-session count. `--no-start` reads an existing daemon and
returns an error if none is running.
