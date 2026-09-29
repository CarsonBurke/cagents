---
name: cagents-agents
description: Delegate work to daemon-hosted Claude, Codex, or Oh My Pi agents, inspect structured results, and coordinate follow-up prompts through the cagents CLI. Use when the user asks to create, oversee, or coordinate agents in cagents.
metadata:
  cagents-bundle-version: "1"
---

Use the same `cagents --json agent` commands from native chats and terminal sessions. The daemon owns execution even with no application window open. Ask the user for missing task scope; do not fabricate a repository, account, model, or permission policy.

1. Choose an absolute repository and working directory. The repository must already be open in cagents. Use an existing channel ID when given; otherwise acceptance creates a channel. Choose `claude`, `codex`, or `omp`. An omitted account resolves the project default once, before acceptance; `--account PROVIDER/system` selects its system configuration explicitly.
2. Submit one task with a unique, stable idempotency key:

   ```sh
   cagents --json agent spawn --provider codex --repository /absolute/repository --directory /absolute/repository/worktree --prompt 'Concrete task and completion criteria' --idempotency-key task-unique-key --surface chat
   ```

   Optional flags: `--channel CHANNEL_ID`, `--account PROVIDER/ACCOUNT_ID`, `--model MODEL`, `--permission-mode MODE`, `--reasoning-effort LEVEL`. Do not weaken permissions just to make unattended work proceed. `--surface terminal` opens a cagents console over the same hosted provider; it is not the provider's stock TUI.
3. Read `data.agent.id`, `data.run.id`, and `data.created` from the JSON response. A lost CLI response may be retried only with the identical request and key. A changed request requires a new key. Do not repeat work merely because the first call timed out.
4. Use `cagents --json agent wait RUN_ID --timeout 60` for bounded waiting, `agent status AGENT_ID` for the latest run, and `agent list` for your agents. Inspect `data.run.state`, `data.run.result.text`, `usage`, `truncated`, and `diagnostic`. `Waiting` is a request for human input, not completion. Open the agent's native controls or terminal console to answer it. Never infer success from terminal output, idle appearance, or absence of an error.
5. After a settled run, send a follow-up with `cagents --json agent send AGENT_ID --prompt 'Follow-up task' --idempotency-key follow-up-unique-key`. Each accepted prompt has a distinct run ID. Do not send more work to an `Interrupted` run: its effects may already have happened. Inspect and explicitly close/reconcile the old session before deciding what to do next.
6. When finished, use `cagents --json agent close AGENT_ID`. Close waits for confirmed teardown; do not bypass a failure by starting another writer on the same conversation.

The daemon supplies session-bound delegation authority automatically. Never print, copy, unset, or fabricate `CAGENTS_AGENT_AUTHORITY`, and never pass a parent session ID as authority. Agents can inspect and control only their own children; human CLI calls without a token own their separately created agents. Delegation depth, children, and simultaneous work are bounded. Handle capacity errors by waiting for and closing existing work, not by bypassing ownership.

Terminal console input accepts ordinary follow-up prompts, `/interrupt`, and `/answer REQUEST_ID ACTION [ANSWERS_JSON]` using exactly the request, action, and field IDs displayed by the provider. `/quit` detaches the console and leaves the provider hosted. Closing the terminal card uses cagents' normal active-work confirmation.
