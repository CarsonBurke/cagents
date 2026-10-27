# Worktrees from the command line

Use `cagents worktree` to create, inspect, and remove Git worktrees on the selected device. A project selector can be an absolute checkout path, an open project ID, or an unambiguous open project name. Without `--project`, a local command uses its current directory. Remote commands require an explicit project on that device.

```sh
cagents worktree create feature/my-change --project /path/to/project --base main
cagents worktree list --project /path/to/project
cagents --json worktree list --project /path/to/project
```

Creation uses the configured managed-worktree directory. It resolves local branches, tags, and commit IDs without requiring a network fetch; a remote base uses the normal reviewed Git fetch path. `--linear ENG-123` retains that ticket's local worktree context as part of the creation. The new worktree appears in an open project's sidebar without switching focus.

A successful Git creation is retained if later status, setup, or sidebar publication fails. The command returns nonzero with the exact creation ID and path. An uncertain Git result also returns its operation ID, proposed path, state, and available residual evidence. Inspect those details and the worktree inventory before retrying; a missing response does not prove rollback.

## Setup and recovery

Setup reads `.cagents/worktree.toml` from the actual primary checkout, including projects with separate Git metadata. Creation runs the exact previously trusted commands and configured copies. Changed commands require a fresh review. Use `--no-setup` to skip both copying and commands for a creation.

```sh
cagents worktree create feature/my-change --project /path/to/project --no-setup
cagents worktree setup list --project /path/to/project
cagents worktree setup review /path/to/worktree --project /path/to/project
```

A review reports the retained creation and run IDs, current copy patterns, commands, fingerprint, and trust state. To start a fresh run, supply that exact creation, expected run, and current configuration fingerprint. `--trust` approves only that reviewed configuration:

```sh
cagents worktree setup resume /path/to/worktree --project /path/to/project \
  --creation CREATION_ID --expected-run RUN_ID --fingerprint FINGERPRINT --trust
```

Close the retained setup card before resuming. Commands are never automatically replayed after a timeout, restart, or lost owner. If the original setup family still lacks proved cleanup, both resume and skip remain refused. Keep the checkout and its retained owner while cleanup settles.

To explicitly skip a nonrunning setup, supply its creation and expected run. Skip performs no configuration parsing, trust change, copy, or command execution, so it also works when the current setup file is invalid:

```sh
cagents worktree setup skip /path/to/worktree --project /path/to/project \
  --creation CREATION_ID --expected-run RUN_ID
```

An optional `--spawn-agent PROVIDER` starts an agent in the created worktree only after setup succeeds. `--model`, `--reasoning-effort`, and `--prompt` configure that agent. A setup error, uncertain startup, nonzero exit, or elapsed waiting deadline retains the worktree and starts no agent.

## Removal

```sh
cagents worktree remove feature/my-change --project /path/to/project
cagents worktree remove /path/to/worktree --project /path/to/project --force
cagents worktree remove feature/my-change --project /path/to/project --delete-branch
```

Removal preserves primary, locked, unavailable, busy, and unproved-setup checkouts. The default also preserves local files and unpublished history. `--force` confirms discarding local content; it does not override live ownership, editors, generation checks, or uncertain setup cleanup. Detached unpublished history receives a rescue branch before the checkout is removed.

`--delete-branch` separately requests Git's safe branch deletion. It never inherits `--force`. A branch deletion failure returns nonzero while retaining the committed worktree-removal result and the branch details.

JSON responses use `schema_version: 1`. Unknown dirty, upstream, or session information is `null` with an associated error where available; an unavailable checkout does not hide healthy rows from the inventory.

## Agent scope and uncertain ownership

Managed agents can use this surface only for their previously accepted project. The daemon verifies the original live launch-directory pin and the accepted project key, then retains and rechecks the selected common directory throughout the operation. Redirecting the checkout's `.git` to another registered project does not grant access to it. A legacy or script session without an accepted project binding receives an explicit refusal; its existing capabilities are unchanged. An accepted Folder identity is not silently converted into authority over newly discovered Git metadata; this surface requires the accepted canonical Git project key. This proves the registered project at admission, not a historical Git-common inode at provider launch.

The original setup holder retains the exact inactive generation and its directory protection before acknowledging readiness. Only that existing holder can persist a reservation after the original startup commits, and it does so before creating a setup process. Losing the daemon before that handoff cannot leave an ownerless reservation. Losing the holder after reservation remains uncertain and blocks replay, skip, and removal until original cleanup is proved; an empty process list or a free lock is not a cleanup proof. Preserve its journal and administrative files for inspection. A checkout recreated by another store with a generation absent from this daemon's journal likewise requires inspection before removal.
