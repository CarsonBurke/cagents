# Worktree setup

A project's primary checkout may contain `.cagents/worktree.toml`:

```toml
copy = [".env", ".env.*", "config/**/*.pem"]
setup = ["pnpm install", "pnpm build"]
```

`setup` also accepts one string. Each entry is a shell command; entries run in
order and stop after the first failure. The UI and CLI use the same preparation
service and launch commands in an interactive terminal for the new checkout.
The worktree row reports the terminal's running, succeeded or failed state.

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

Choose **Skip setup** for one creation, or use `cagents worktree create --no-setup`.
Skipping bypasses both copies and commands. A failed or cancelled setup keeps the
created worktree and any completed copies for inspection; it never automatically
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
