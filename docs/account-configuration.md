# Configuration for additional accounts

The Accounts pane creates a separate provider home for every added login.
Existing system and linked homes are left unchanged.

## Reading usage from the CLI

```sh
cagents account info --json
cagents account info --provider codex --json
cagents account info --account codex/system --refresh --json
```

`account info` reads account registry metadata and the usage observations the
selected daemon already has. It does not sign in, inspect credential files,
start a provider conversation, or probe providers by default. OMP allowances
come from its bounded, read-only `usage_history` query. The human output is
concise; `--json` uses the normal stable CLI envelope and includes each account's
ID, provider, registered and observed identities, plan, allowance windows,
credits, source, observation time, age, and stale/reset indicators.
Reset inventory includes each grant's scope, count, expiry where reported,
and whether it can be used now.

`--refresh` requires one explicit `--account` ID and reuses that provider CLI's
isolated usage probe. It requests usage, without submitting a paid model turn.
A busy or failed probe retains the previous observation and reports the reason.
OMP uses its recorded usage and does not support this probe flag. Account IDs
are distinct from email addresses: two homes can report the same email without
becoming the same configured account. A changed observed login is reported
separately from the registered identity.

Inside a live cagents session, the report also includes authenticated caller
latest reported context occupancy, its reported context limit, and the account selected by the
session's captured effective execution environment. It does not substitute the default for new chats. Missing execution metadata or
API credential overrides leave subscription account selection unknown.
Providers which do not report context occupancy or limits leave those fields
null. Terminal callers also leave account binding unknown, because foreground
provider activity does not prove the environment it currently uses. All cached
account allowances remain available. Occupancy can lag while a turn is running; the daemon query time is not
the time that occupancy was observed. A CLI outside a hosted session has no caller context. A running account
which has been archived remains readable and is marked `archived`; explicitly
reading that account does not make it selectable for new work.

Remaining plan fractions come from provider-reported usage windows. Expired
resets and stale observations remain marked, without assuming a renewed
allowance. Extra usage retains the provider's direct balance where available;
derived monetary remaining amounts require identical currency and exponent
for the reported limit and usage. Unlimited, missing, or incompatible values
remain unknown. Conversation token counts and API-equivalent cost estimates
are never converted into subscription plan allowance.

## Using a usage reset

The account usage menu and Accounts pane show available resets. When a covered
window is near or at its limit, **Use a reset** asks you to confirm the account,
the windows it refills, and how many resets are left. A matching chat limit card
offers the same action. A model reset only refills the windows its grant names;
a Fable reset does not necessarily refill the account's weekly allowance.

From a system terminal, use the stable account ID printed by `account list`:

```sh
cagents account reset claude/system --scope Fable
cagents account reset codex/system --yes
```

The first command asks for confirmation. `--yes` explicitly confirms spending
one reset. Without a terminal, confirmation requires `--yes`. If several resets
cover different windows, select the scope rather than accepting an ambiguous
choice. Spending a reset cannot be undone. Agent-authenticated sessions cannot
spend resets.

Each attempt has an idempotency key. If the result is uncertain, cagents reads
the account again and shows the reported state. It never automatically retries
a consume request. Keep the reported key for any deliberate retry of that same
attempt; reuse both `--reset-id ID` and `--idempotency-key KEY` from the printed
recovery command rather than creating a new operation.

Codex uses its supported app-server reset-credit methods. Claude Code does not
expose a reset operation through its control protocol, so Claude reset inventory
and consumption use the private endpoint its own client uses. This narrowly
scoped adapter reads the selected Claude home's OAuth login without copying or
refreshing credentials. Normal sign-in remains the provider CLI's responsibility.

## Choosing defaults from the CLI

`cagents account list` prints the stable IDs accepted by
`cagents account default claude claude/ACCOUNT-UUID`. Use `claude/system` to
pin the system account, or `cagents account default claude automatic` to restore
automatic routing. These commands use the selected daemon, including `--device`;
the client never supplies an account home or replaces the account registry.

The daemon checks the current registry revision and refuses removed, archived,
wrong-provider and known conflicting accounts. A concurrent edit requires a
fresh explicit command; the CLI does not retry a failed preference write.
Agent-authenticated callers cannot change defaults. Omp chooses its own accounts
and accepts only `automatic`. Existing conversations keep their recorded account.

New installations use **Automatic** for each provider. Add each subscription
once with its provider's browser or device-code sign-in. Thereafter the CLI
uses its saved login and refreshes its own tokens. The Claude reset adapter
described above is the only direct credential reader; cagents does not copy
credentials. Multiple personal and organization plans can remain
signed in at the same time in separate homes.

To refresh an existing login, choose **Sign in again** in its account menu.
Local Codex accounts also offer **Sign in again with a code**, which keeps
the same home and works without an available local browser callback port.
Finish or close an account's current sign-in terminal before starting another.
Re-login stays watched while its terminal runs, even during a long browser
handoff; after it finishes, cagents verifies the account before accepting its
new identity. This explicit re-login intent belongs to the window that started
it; reopening a window does not authorize an unexpected change of identity.

Automatic keeps using the current account until the provider reports that its
allowance is exhausted or its login needs attention, then chooses an account
with a recent successful quota report. Unknown, stale, reset, conflicting and
archived accounts are excluded. Of eligible accounts, it chooses the largest
reported remaining allowance, breaking ties consistently. New native chats,
shell terminals and unpinned CLI agent launches use the same policy.

Selecting an account in a chat pins that conversation, even if it was already
using that account. Selecting a provider default or a project account pins
new work in that scope. Choose **Automatic** in Settings → Accounts to restore
automatic provider routing; clear a project choice to inherit it. Existing
saved conversations retain their previous account.

For an automatic native chat, the next unsent message can switch accounts
between turns. The daemon fences the exact idle actor, transfers only that
conversation's transcript to the alternate home, and requires the provider
to confirm the same conversation before saving the change and sending the
untouched message. Busy conversations, queued work, approvals and other live
owners prevent switching. A background subagent that is still running, or
waiting on its own background work, also prevents it: the message stays unsent,
and the conversation names the subagent to stop from its tab first. A failed or partially executed turn is never replayed.
Missing, divergent or unsupported history pauses the switch and preserves the
unsent message and original saved conversation. Omp handles its own account
rotation. Shell terminals retain their launch homes for their lifetime.

Device windows read quota and account configuration from their selected
daemon, save account preferences there, and run sign-in on that device.
Remote Codex sign-in uses its device-code flow, so browser consent can finish
on the desktop without forwarding localhost callbacks. Adding, linking and
removing managed accounts operates on the daemon's device. Linked paths are
entered as device paths; `~/` expands to the device user's home. The daemon
refuses removal while that home still belongs to a live hosted terminal,
sign-in or conversation. Linked homes are forgotten without deleting their
files. Pending sign-ins survive a window restart and are verified on the
device before joining its account list.

New Claude homes receive a one-time copy of `settings.json` and user-scoped
`mcpServers`. This preserves hooks, permissions, status-line settings and MCP
definitions while allowing Claude to replace its settings file normally.
Subsequent edits are independent, except that new native chats in managed Claude
homes use the current system `permissions.defaultMode`, rather than the copied
account default. Explicit chat or agent permission choices and project settings
take precedence. Existing conversations keep their selected mode; linked Claude
homes keep their own defaults. Instructions and extension directories remain
linked. Provider login data and project trust state are not copied. User-authored
settings and MCP definitions can themselves include environment values or headers;
those configuration values remain in the private local account home.

Claude's default user MCP file is `~/.claude.json`. With `CLAUDE_CONFIG_DIR` set,
it is `.claude.json` inside that directory. Only the top-level `mcpServers` map
is deserialized into the new file. Missing configuration is fine; unreadable or
malformed MCP configuration stops account creation with an error rather than
silently creating an account with different tool configuration.

New Codex homes share `config.toml` by default. This includes model and MCP
configuration, but also `forced_login_method` and `cli_auth_credentials_store`.
Editing a shared file affects every account linked to it. In **Add account**, select
**Give new Codex accounts their own config copy** before starting either Codex
sign-in flow to create an independent copy instead. This does not alter existing
accounts. A copied file initially retains the same login settings; it can then be
edited independently.

The shared configuration does not merge Codex's login stores. Codex 0.157.1 derives
its keyring entry from the SHA-256 digest of the canonical `CODEX_HOME` path.
Cagents allocates distinct real directories for managed homes, so even accounts
using the same keyring setting retain separate entries. Cagents does not force a
different credential-storage method or read credential values to verify this.

Provider references checked against the installed CLI versions:

- [Claude settings and configuration locations](https://code.claude.com/docs/en/settings)
- [Claude user-scoped MCP configuration](https://code.claude.com/docs/en/mcp)
- [Codex 0.157.1 keyring storage implementation](https://github.com/openai/codex/blob/rust-v0.157.1/codex-rs/login/src/auth/storage.rs)

## When the account list is lost

Each added account keeps its login and conversation history in its own home
under `~/.local/share/cagents/accounts/`. The account list in
`~/.config/cagents/accounts.toml` only names those homes, so a reset or
replaced configuration directory can lose the list while every home is intact.

The daemon lists such homes again by itself, at startup and about once a
minute while it runs. It first asks the account's provider CLI for its
current sign-in; cagents never reads credential files to decide. A home is
listed again under its original key, so saved conversations pinned to it
resume where their history is:

- A home whose provider confirms a sign-in is listed again.
- A home whose login matches the system account or another listed account is
  restored as archived, so the same login is not offered twice. Its
  conversations still resume.
- A home whose provider reports no sign-in, such as a sign-in that never
  finished, is left exactly as it is.
- A home left by a sign-in that was not kept, because it duplicated a listed
  account or was cancelled, is not an account the list lost. The list records
  it, and it is left as it is while that record remains.
- Partly removed homes (`.removing-*`), linked homes, and homes under an
  account list written by a newer cagents are never touched.
- If the provider cannot be asked, for example because it timed out, the
  home is checked again later, at longer intervals each time it fails.

A home being checked does not hold up shutting down or upgrading the daemon,
and it does not stop other conversations from opening.

If a conversation's account is missing before the daemon gets to it, the
conversation does the same check right away. A saved conversation then
continues on its own account. A new conversation sends its first message,
unless you changed the message or chose another account in the meantime. If
the check finds no sign-in, the conversation says so and offers another
account. If the home's files are gone as well, the conversation says the
account was removed and offers another account instead.

## Registering an existing account home from the CLI

Link one existing Claude or Codex home on the selected device:

```sh
cagents --device local account link codex '/absolute/path/to/selected-codex-home'
cagents --device fw16 account link codex '~/account-homes/selected-codex'
```

The path belongs to the selected device. Quote `~/...` for remote homes so
your local shell does not expand it. The daemon validates that the directory
belongs to its user and reserves it against conflicting live activity. This
registers the existing home without copying files, modifying configuration,
changing defaults or replacing another account. Omp manages its own logins.
Hosted agent credentials cannot authorize account lifecycle changes.

The result reports **registered**. Authentication and execution readiness are
explicitly **not checked**, including for a credential-only home with no
`config.toml`. Use the returned account ID on the same device to ask its
official provider CLI for its current identity and usage:

```sh
cagents --device fw16 account info --account codex/ACCOUNT-UUID --refresh
```

Check that the observed provider identity matches the intended account before
selecting it for work. A successful identity/usage probe does not establish
workspace write access, network access, or GitHub/cagents availability. Keep
the intended execution configuration separate from authentication and verify
those capabilities in the actual coding session; linking never grants them.

This command links a home already present at the destination. Cross-device
credential transfer and browser-session import are not implemented by this
command. Use the provider's supported sign-in flow on the destination when
needed. If a reply is lost or unclear, inspect `account list` on the same
device before retrying; linking does not automatically retry.

## Removing a linked account registration

Use the stable account ID from `cagents account list` on the selected device:

```sh
cagents account remove claude/00000000-0000-4000-8000-000000000001 --yes
```

The selected daemon authenticates the caller and applies its existing account
lifecycle admission. Agent tokens and live home owners cannot remove
accounts. System accounts cannot be removed. This build also refuses managed
account file deletion and retains the account and its files. A linked account
is forgotten without deleting its files. The CLI sends only the account ID,
never a local home path, and requires `--yes` before contacting the daemon.
If the response is uncertain, inspect the account list before retrying; removal
does not retry automatically.
