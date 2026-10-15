# Worktree setup core

This backend foundation provides configuration, copy preparation, trust and durable
intent APIs. UI and CLI integration, interactive setup terminals, row status and
explicit resume controls are pending companion changes. The examples below define
the shared backend contract; they do not enable setup automatically.

A project's primary checkout may contain `.cagents/worktree.toml`:

```toml
copy = [".env", ".env.*", "config/**/*.pem"]
setup = ["pnpm install", "pnpm build"]
```

`setup` also accepts one string. Each entry is a shell command; entries run in
order and stop after the first failure in the returned shell recipe. Planned UI
and CLI consumers will use this preparation service, launch that recipe in an
interactive terminal and display its running, succeeded or failed state.

The first command launch requires explicit trust for the project. Trust is saved
in this clone's local Git configuration and applies to the exact contents of the
setup file. Editing the file requires reviewing it again. A file cannot grant
trust to itself. Copy-only configurations do not require command trust.

Copies come from the primary checkout, preserve ordinary permission bits, and
never overwrite tracked files (even deleted tracked files), existing untracked
files, or symbolic links. Sources missing from a pattern are reported and do not
fail creation. Patterns must stay within the checkout and cannot select `.git`.
Directory traversal and copies refuse symbolic links and do not cross into nested
Git checkouts, including other managed worktrees or the destination itself. Glob patterns support
`*`, `?`, character classes and recursive `**` directory components.

Copy output stays in an unnamed Linux file until publication from its retained
descriptor. Filesystems must support `O_TMPFILE` and linking that inode through
`/proc/self/fd`; unsupported operations fail explicitly, without a named temporary
fallback. The anonymous inode is created in the retained checkout before any
missing copy directories are created. A copy cannot cross to a different
filesystem mounted beneath that checkout. Failures can leave newly created
directories or earlier completed copies; preparation is not one atomic operation.

The preparation API accepts a per-creation Skip choice, which bypasses both
copies and commands. The planned **Skip setup** UI and
`cagents worktree create --no-setup` CLI flag are not available in this core PR.
A failed or cancelled preparation keeps the created worktree and any completed copies for inspection; it never automatically
deletes work. Callers expose progress, errors and terminal cancellation.

Preparation bounds config files to 64 KiB, configurations to 128 copy patterns
and 32 commands, scans to 100,000 directory entries, copies to 10,000 files and
individual files to 64 MiB. Exceeding a limit reports an error and keeps the
checkout. Existing production/development sessions are unaffected by setup.

Creation requests can retain `WorktreeSetupIntent`: the reviewed plan, primary
checkout, Skip choice, setup run identity, and completed-copy checkpoint. Legacy
requests default to no intent. This metadata never authorizes automatic shell
replay after restart or an uncertain creation result.

`RepositoryService::update_worktree_setup_intent` changes only a settled Created
journal. Callers supply the exact prior intent and the updated intent; both this
expectation and a whole-payload compare-and-swap must match. A stale callback
cannot overwrite a newer resumed run. This method performs no Git, filesystem or
shell effects; callers still verify configuration/trust before explicit setup.

Copies preserve source access ACLs as well as mode bits and remove inherited
access ACLs when the source has none. An ACL-copy error prevents publication of
the temporary copy; the created worktree is retained for inspection.

`worktree_setup_creation` reads one settled creation through the same identity
checks without reconciling or rewriting it. It returns no row when absent and
an error for invalid evidence; resume callers verify the current checkout too.

The copy restores the source group and replaces inherited ACL identities while
its file is still unnamed, before applying source mode bits. This
also avoids granting a setgid destination's different group access to secrets.
If the source group cannot be preserved, setup stops and keeps the worktree.

Glob traversal is bounded to 64 directory levels as well as the existing entry
and copy limits. A deeper selection fails setup before copying files; the new
checkout remains available.

## Managed directory

Local and device creation default to `.worktrees/` beneath the checkout. An
explicit `managed_worktree_directory` setting keeps its configured location.
Before creating inside the primary checkout, cagents adds an anchored rule to
the clone's common `info/exclude`, using Git's lock-and-rename convention. It
keeps existing rules and skips the write when Git already ignores the directory
or the managed location is outside the primary checkout. Tracked `.gitignore`
is unchanged.

A destination override excludes only that exact checkout, so other files in a
shared parent remain visible. If a higher-priority `.gitignore` rule explicitly
keeps the destination visible, creation stops and asks you to adjust that rule.

Existing registered worktrees under the old `worktrees/` directory stay in
place. The next worktree creation excludes that legacy directory too; changing
the default does not move or delete existing worktrees.
