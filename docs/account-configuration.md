# Configuration for additional accounts

The Accounts pane creates a separate provider home for every added login.
Existing system and linked homes are left unchanged.

New Claude homes receive a one-time copy of `settings.json` and user-scoped
`mcpServers`. This preserves hooks, permissions, status-line settings and MCP
definitions while allowing Claude to replace its settings file normally.
Subsequent edits are independent. Instructions and extension directories remain
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
