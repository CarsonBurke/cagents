# Hosted agent execution

`cagents --json agent` creates daemon-owned Claude, Codex, and Oh My Pi conversations without requiring a window. Each conversation has an agent ID; each accepted prompt has a separate run ID. Opening cagents later adopts the conversation into its specified project/channel without changing the window's current focus.

```sh
cagents --json agent spawn \
  --provider codex \
  --repository /absolute/repository \
  --directory /absolute/repository/worktree \
  --prompt 'Review the current change and report concrete findings' \
  --idempotency-key review-2026-09-26 \
  --surface chat
```

The repository must already be open in the daemon's project registry. `--channel` selects an existing channel owned by that repository; omitting it creates a channel in the same acceptance transaction. `--directory` is explicit and may select a worktree. Account, project, and working-directory errors never fall back to a different target.

Scheduled callers can retain an approved directory descriptor. Provider startup uses that descriptor as its working directory, and execution-authorizing commands check that the configured path still identifies the approved directory. This is a dispatch-boundary identity check, not a filesystem sandbox: provider RPCs may resolve paths themselves. Replacing or removing the directory prevents new prompts and approvals but does not prevent interruption, denial, or cleanup.

`--account PROVIDER/system` selects the system configuration. A managed or linked account uses `PROVIDER/UUID`. Omitting the flag resolves and records the project default at acceptance. Later default changes do not change that accepted request's account. Model and permission options are passed through the existing native launch policy before the first prompt:

| Option | Claude | Codex | Oh My Pi |
| --- | --- | --- | --- |
| `--model` | `--model` | New thread `model` | `--model` |
| `--permission-mode` | `--permission-mode` | New thread `approvalPolicy` (`--ask-for-approval`) | `--approval-mode` |

Permission values are provider-specific. The command does not broaden permissions automatically. If omitted, the selected provider configuration remains authoritative. Follow-up prompts reuse the hosted conversation and preserve its current controls.

## Results and retries

Successful JSON responses use the regular CLI envelope. Spawn/send return `data.agent`, `data.run`, and `data.created`; status/wait return `data.agent`, `data.run`, and `data.timed_out`; list returns `data.agents`.

```sh
cagents --json agent status AGENT_ID
cagents --json agent wait RUN_ID --timeout 60
cagents --json agent list
cagents --json agent send AGENT_ID --prompt 'Investigate the first finding' --idempotency-key review-followup-1
cagents --json agent close AGENT_ID
```

A run moves from `Accepted` to `Running`, may enter `Waiting`, and finishes as `Succeeded`, `Failed`, `Cancelled`, or `Interrupted`. Waiting returns promptly so a caller can route the approval/question to a person. It is not successful completion. The result contains provider-correlated text, usage, an optional diagnostic, and an explicit truncation flag. Text is bounded to 1 MiB; diagnostics to 16 KiB. Terminal output is never scraped to infer completion.

Acceptance and its dispatch claim commit to the daemon's SQLite registry with `synchronous=FULL` before spawning a provider or submitting a prompt. An identical request with the same caller and idempotency key returns the original IDs; it does not send the prompt again. Reusing a key for different content fails. Retry a lost response with the identical request and key.

If a daemon restart loses proof of an accepted prompt's outcome, the run becomes `Interrupted`. Its external effects may already have happened. It is never resent automatically and retains its concurrency reservation until explicitly reconciled/closed. A private pending launch record preserves ownership if a process started but its final descriptor could not be committed. Such a record is not exposed as an adoptable UI session. A startup failure that retains a known hosted session reports that descriptor; inspect or close it before deciding how to proceed.

## Console surface

`--surface terminal` creates a real hosted terminal running a cagents console over the same native provider actor. It does not launch another provider process and does not emulate the provider's stock TUI. The console streams a bounded transcript, structured usage, run outcomes, approvals, and questions.

- Ordinary input submits a new durable run.
- `/interrupt` requests interruption through the native adapter.
- `/answer REQUEST_ID ACTION [ANSWERS_JSON]` answers only the explicit displayed request/action. The optional JSON is an array of `{"field":"FIELD_ID","values":["VALUE"]}` objects.
- `/quit` detaches the console; the provider remains hosted.
- Closing the terminal card follows normal active-work confirmation and retires the exact paired actor.

## Delegation and bundled skill

Fresh hosted sessions receive a daemon-issued, session-bound capability in `CAGENTS_AGENT_AUTHORITY`. It is transient process context, omitted from public recipes and redacted in diagnostics. Agents use the same CLI commands as people, but can only inspect/control children created under their live, proved session. A plain human CLI invocation without that variable owns a separate root scope. Passing a parent ID never grants authority. Do not copy, print, unset, or fabricate capabilities to bypass that scope.

The service limits each parent to 64 open children, 8 concurrent/uncertain runs, a maximum delegation depth of 8, and 32 concurrent/uncertain runs globally. Close finished conversations when they are no longer needed. Explicit retirement revokes the issuing capability; proof-changing adoption must validate the retained exact session lineage.

The packaged `cagents-agents` skill teaches this CLI workflow. Before a new provider/terminal process starts, cagents installs it without replacing user content and prepends the matched CLI directory to that child's PATH. Claude uses the selected `CLAUDE_CONFIG_DIR/skills` (or `HOME/.claude/skills`); Codex and OMP use `HOME/.agents/skills`. Installation occurs under the account launch lease and never recreates a removed selected home. Existing identical content is reused; conflicting content is preserved and reported in daemon diagnostics with its path. Provider discovery opt-outs still apply.

These locations follow the providers' documented skill discovery: [Claude skills](https://code.claude.com/docs/en/skills), [Codex skills](https://learn.chatgpt.com/docs/build-skills), and [OMP skills](https://github.com/can1357/oh-my-pi/blob/main/docs/skills.md). The same installed instructions appear in cagents' native filesystem skill catalog and provider terminal sessions.
