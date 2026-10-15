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
archiving, moving and deleting managed accounts operates on the daemon's device. Linked paths are
entered as device paths; `~/` expands to the device user's home. The daemon
refuses removal while that home still belongs to a live hosted terminal,
sign-in or conversation. Linked homes are forgotten without deleting their
files. The same daemon guard applies to local windows, including idle hosted
conversations and panes open on that home. **Archive** is the default removal
action: the provider signs out, the account is hidden from routing, and all
transcripts and files remain on disk. Link that home again to restore it.
The dialog counts saved conversations and offers **Move conversations to … and
archive** to copy every provider transcript to another account of the same
provider before sign-out. Divergent history is never overwritten. Saved
conversation descriptors keep their original home until an explicit account
switch confirms the same conversation on the replacement. **Delete files
permanently** is a separate action requiring the account name; it deletes the
home's conversations, caches, and app-retained original prompts and attachments, and cannot be undone. Archive and Move retain that original-prompt data. If the home was already removed externally, deletion uses a prior physical-home association where available; otherwise the result explicitly says that removal of older retained prompt data could not be verified.

Pending sign-ins survive a window restart and are verified on the
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


Started native conversations also expose the account picker. An explicit choice
pins the conversation to the replacement account. When a turn, approval or
queued message still owns work, the change waits for idle and shows “Switches
to … after this turn”. At the idle boundary the daemon fences the exact actor,
copies only its confirmed transcript and requires the same provider conversation
identity before saving the replacement. A system transcript notice records the
switch. If the transcript cannot be transferred, **Continue on … as a new
conversation (cache lost)** opens a separate draft seeded with a bounded recent
transcript excerpt and the unsent request; the original conversation is retained.
The draft is editable and is not sent automatically.

Automatic routing is maintained by the daemon while hosted agents exist,
independent of window focus. Candidate accounts are probed at most once per
30 minutes plus up to one minute of jitter. On exhaustion, stale alternatives
are refreshed within one 12-second deadline before the send reports no available
account. Windows adopt newer daemon snapshots from the shared cache before
scheduling their own focused display refreshes.

Provider history moves retain Codex plain and compressed rollout files, including earlier rollout versions referenced by a conversation. The displayed count is distinct provider conversation IDs. A conflicting plain/compressed representation on the destination is refused before copying. Archived homes retain the existing explicit saved-conversation resume behavior; they stay excluded from automatic routing and may require signing in again or choosing another account.
