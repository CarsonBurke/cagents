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
uses its saved login and refreshes its own tokens; cagents never reads or
copies credentials. Multiple personal and organization plans can remain
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
owners prevent switching. A failed or partially executed turn is never replayed.
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

## Checking an account's default execution policy

Check one explicit Codex account and workspace on the selected device:

```sh
cagents --device fw16 account check --account codex/ACCOUNT-UUID \
  --workspace '~/project' --expect-email selected@example.com
cagents --device fw16 --json account check --account codex/ACCOUNT-UUID \
  --workspace '~/project' --expect-email selected@example.com \
  --write --github-repository OWNER/REPO
```

Quote `~/...` so the selected device expands it. The command accepts an absolute
workspace or that device-relative home spelling, one Codex account ID, the
expected provider email, and the two optional capability checks. It accepts no
provider-home override, environment values, arbitrary command or policy override.
It authenticates the caller to the selected daemon and submits one check without
automatic retries.

Executable lookup uses the application's already discovered shell search path,
or the daemon's inherited `PATH` when discovery has not completed. This check
does not start an interactive shell to discover another executable location.

Authentication, the reported default execution policy, workspace write access,
and GitHub repository access are separate observations. The official Codex client
reads its account and effective configuration without starting a model turn.
`--write` explicitly permits one fixed scratch write/read/remove in the supplied
workspace under that default policy; cleanup is reported separately, including
unknown cleanup after an interrupted or missing helper receipt. Without `--write`
the command does not request a scratch file. `--github-repository OWNER/REPO`
explicitly permits one GitHub repository read under the same policy. Reported push
permission is metadata; no push or repository mutation is performed. Omitting
that option does not request the endpoint check.

Provider cleanup is separate from scratch cleanup: success requires confirmation
that the owned process group accepted its termination signal (or was absent) and
the original provider leader was reaped. An uncertain cleanup keeps the account
home and workspace occupied for destructive operations; it does not prevent
ordinary launches. A failed group signal stays quarantined rather than allowing
a later observation of leader exit to authorize directory removal.

JSON uses the normal CLI envelope and retains each observation's status/reason,
the selected account, requested and resolved workspace, expected identity,
observation time, daemon proof, physical identities, and executable identity.
An unsupported or failed measurement remains a scoped report, with `outcome`
`failed` and exit status 4; it is not silently converted into a protocol failure
or a claim that authentication failed. A refused request, lost reply or malformed
response remains a command error and is not retried.

The scope is **account default policy at the observation time**. A pass is not
authorization for the current coding actor or a future launch, and it does not
test cagents authority, import credentials, transfer browser sessions, change
configuration/defaults, resume work automatically, or prove broader account
readiness. Existing actors retain their own captured permissions and network
policy. Linking a home, confirming its login and checking this default policy
remain distinct operations.

If the result reports a read-only default, you can request file edits for a new
conversation through **Permissions → Workspace write**. This leaves account
defaults unchanged, so the default-policy check can still report read-only.
Existing conversations and network access have separate policies; this choice
does not configure unattended agents.

## Creating missing coding defaults

An imported home can authenticate successfully while having no `config.toml`.
After **Check coding access** confirms the selected login and provider cleanup,
choose **Inspect coding defaults**. If that registered Codex home's configuration
is absent, **Create coding defaults** offers workspace write with on-request
approvals. Network access starts unchecked and requires an explicit choice.

This creates an independent file on the selected device. Existing entries,
System accounts and homes or configuration targets shared with another account
are preserved. Busy accounts and uncertain independence refuse creation. No
registration, account default, credential or existing conversation is changed.
Setup also refuses a directory writable by other users; it does not change
the directory's permissions.

The configuration applies to new Codex processes. Run **Check** again to measure
actual access; creation alone does not prove execution or network readiness.
Closing the form leaves an outstanding request running and its result appears
in a notice. If creation cannot be confirmed, inspect the account configuration
before another request. Cagents does not retry or roll back an uncertain creation.

For headless setup, use the same selected-device flow through the CLI:

```sh
cagents --device workstation account create-coding-defaults \
  --account codex/00000000-0000-4000-8000-000000000001 \
  --workspace '~/projects/example' --expect-email you@example.com
```

This checks the selected login without a scratch write or GitHub request, then
inspects and creates only an absent configuration. Add `--allow-network` to
include network access; it is disabled otherwise. The command preserves inherited
caller authority, so hosted agents cannot use it to change their permissions.
Existing entries are preserved. An uncertain creation is not retried: inspect
the same account on the same device before another request. Created defaults
apply to new Codex processes and do not establish actual execution readiness.

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

## Importing a staged ChatGPT cache

In **Accounts → Add account → Import staged ChatGPT cache**, choose one staged
`auth.json` on the selected device. Enter the expected ChatGPT email and confirm
both the file copy and the session-sharing limitation. **Inspect file** checks
its format and private-file properties. **Prepare import** creates a separate
managed home and copies only that file; it does not register a login or start a
coding task. Existing homes, identities, defaults and configuration are preserved.

**Verify identity** asks the official Codex client in that original destination
home to report its ChatGPT identity. A wrong, missing, API-key or duplicate
identity does not select another account. The pending import is retained for
inspection. Successful registration can then create an absent, independent
workspace-write configuration if you explicitly requested it; network access
requires its own choice. Existing configuration is never overwritten.
Registration and configuration creation do not prove coding access: use
**Check coding access** with the intended workspace and explicit write/GitHub
checks before relying on a worker. This flow does not dispatch or resume tasks.

Closing the form preserves an outstanding operation. After restarting the app,
use **Load retained imports** and resume its original operation. **Status** reads
metadata; **Verify identity** is always explicit and uses the current daemon
connection while preserving the original account key, email and intent. An
uncertain result does not trigger another copy, a new operation or an automatic
retry. Completed operations remain retained; this initial journal holds at most
16 total operations, including completed imports. Completing an import does not
free a slot, and this flow provides no journal-deletion operation.

The CLI uses the same selected-device service from the user's own terminal:

```sh
cagents --device workstation account import inspect '~/staged/auth.json'
cagents --device workstation account import prepare --request-file ./import-plan.json
cagents --device workstation account import list
cagents --device workstation account import status ORIGINAL-UUID
cagents --device workstation account import verify ORIGINAL-UUID
```

Inspection returns a metadata-only plan preview. To prepare, supply its complete
observation and destination proof, an original nonnil operation UUID, expected
email, explicit copy acknowledgements, and chosen profile in a private JSON file.
The request file must contain metadata, never credentials. Preserve the operation
UUID if a response is lost. Status and List never advance an import or verify
execution access. Hosted agents cannot import credentials or change their own
permissions through these commands.

This is a destination import for an already staged **managed ChatGPT file
cache**. It does not export another device's login, copy a whole account home,
read browser cookies or perform browser authorization. Codex documents copying a
file cache as a headless setup option; it also warns that managed refresh tokens
must be used on one machine or one serialized stream. Concurrent copies can
invalidate each other's refresh state. Use the provider's supported transfer and
consent path, protect the staged file, and never replace the client's refreshed
cache with an older seed. See the official [authentication guide](https://learn.chatgpt.com/docs/auth)
and [managed authentication guidance](https://learn.chatgpt.com/docs/auth/ci-cd-auth).
