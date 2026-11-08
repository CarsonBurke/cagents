"""Invented words for the workspace: project names, prompts, replies, code,
command output, and the harness noise that opens real conversations.

Nothing here is copied from a real conversation. To add a kind of noise, add
an entry to NOISE; profiles draw from every entry.
"""

PROJECT_NAMES = [
    "acme-api", "web-dashboard", "ml-pipeline", "billing-service", "mobile-app", "infra",
    "search-indexer", "docs-site", "auth-gateway", "data-warehouse", "notification-hub",
    "design-system", "edge-proxy", "payments-ledger", "analytics-etl", "chat-widget",
    "support-portal", "release-tools", "geo-tiles", "feature-flags", "image-resizer",
    "audit-log", "cli-toolkit", "scheduler",
]

BRANCH_WORDS = [
    "fix-auth-refresh", "feat-rate-limits", "chore-deps", "fix-empty-cart", "feat-dark-mode",
    "perf-p99-customers", "feat-webhook-retries", "fix-csv-unicode", "refactor-pagination",
    "feat-eval-split", "fix-tokenizer-speed", "feat-billing-empty-state", "migrate-invoices",
    "fix-flaky-e2e", "feat-keyboard-nav", "upgrade-axum", "fix-memory-leak", "feat-audit-export",
    "fix-timezone-drift", "feat-bulk-import", "fix-race-scheduler", "feat-sso", "docs-runbook",
    "fix-cache-stampede", "feat-search-facets",
]

TOPICS = [
    "Fix the token refresh race in the auth middleware",
    "Add per-tenant rate limits to the public API",
    "Why does /v2/orders return 500 when the cart is empty?",
    "Write integration tests for the webhook retry queue",
    "Migrate the invoices table to the new schema",
    "Profile the slow p99 on GET /customers",
    "Review the pagination cursor encoding change",
    "Upgrade axum to 0.8 and fix the extractor errors",
    "Make the usage chart readable in dark mode",
    "The settings page flickers on load, find out why",
    "Add keyboard navigation to the project switcher",
    "Replace moment.js with date-fns",
    "Build an empty state for the billing page",
    "Fix the CSV export truncating unicode names",
    "Why did last night's training run diverge at step 40k?",
    "Add a held-out eval split for the classifier",
    "Speed up the tokenizer preprocessing step",
    "Compare macro-F1 between the two checkpoints",
    "Find the memory leak in the worker pool",
    "The e2e suite is flaky on CI, stabilise it",
    "Add an audit export endpoint with streaming CSV",
    "Timestamps drift by an hour after the DST change",
    "Write a runbook for the on-call rotation",
    "Add faceted search to the catalog page",
    "Stop the cache stampede when the config reloads",
]

FOLLOW_UPS = [
    "Yes, go ahead.", "Can you also add a test for the edge case?", "Looks good. Commit it.",
    "What about the error path?", "Try a sliding window instead.", "Run the full suite again.",
    "Why did that test fail?", "Keep the old behaviour behind a flag.", "Now open a PR.",
    "Explain the change in two sentences.", "Revert the last change, it broke the build.",
    "Use the existing helper instead of writing a new one.",
]

REPLIES = [
    "I traced it to the retry path: the refresh token is read before the lock is taken. "
    "I moved the read inside the critical section and added a regression test.",
    "Done. The change is in three files; tests pass locally. One open question: should the "
    "limit reset per calendar minute or on a sliding window?",
    "The root cause is an unchecked empty slice in the totals calculation. Fixed and covered by a test.",
    "I measured it: 82% of the time goes to the N+1 query on addresses. Batching it brings "
    "p99 from 1.9s to 240ms.",
    "Here's the plan: 1) add the column nullable, 2) backfill in batches, 3) flip the constraint. "
    "I've written step 1.",
    "The flake comes from two tests sharing a port. Each now binds port 0 and reads the "
    "assigned one back.",
    "The leak is a listener registered on every reconnect and never removed. It's now removed "
    "on drop, and a test counts listeners across ten reconnects.",
]

THOUGHTS = [
    "The failure only appears under load, so it's likely ordering rather than logic. Let me read "
    "the lock acquisition order before changing anything.",
    "Two options: patch the caller, or fix the helper every caller shares. The helper is the "
    "shared cause, so fix it there and check the other callers.",
    "I should confirm the test fails before the fix, otherwise it doesn't prove anything.",
    "The schema change needs a backfill; doing it in one transaction would lock the table.",
]

# Fenced code in several languages. `{name}` is replaced per use so repeated
# snippets are not byte-identical.
CODE = {
    "rust": """pub fn {name}(limits: &Limits, tenant: TenantId) -> Result<Permit, Throttled> {{
    let window = limits.window(tenant);
    let mut state = window.lock().map_err(|_| Throttled::Poisoned)?;
    if state.remaining == 0 {{
        return Err(Throttled::RetryAfter(state.reset_in()));
    }}
    state.remaining -= 1;
    Ok(Permit {{ tenant, issued: Instant::now() }})
}}
""",
    "python": """def {name}(rows, *, batch=500):
    \"\"\"Backfill in batches so the table never stays locked.\"\"\"
    for start in range(0, len(rows), batch):
        chunk = rows[start:start + batch]
        with transaction() as tx:
            tx.executemany("UPDATE invoices SET total_cents = ? WHERE id = ?", chunk)
        log.info("backfilled %d/%d", start + len(chunk), len(rows))
""",
    "typescript": """export function {name}(points: UsagePoint[], theme: Theme): ChartSeries {{
  const palette = theme.mode === "dark" ? darkPalette : lightPalette;
  return {{
    label: "Requests",
    color: palette.accent,
    values: points.map((point) => ({{ x: point.at, y: point.count }})),
  }};
}}
""",
    "go": """func {name}(ctx context.Context, q *Queue) error {{
	for attempt := 0; attempt < maxAttempts; attempt++ {{
		if err := q.Deliver(ctx); err == nil {{
			return nil
		}}
		time.Sleep(backoff(attempt))
	}}
	return ErrGaveUp
}}
""",
    "sql": """-- {name}
ALTER TABLE invoices ADD COLUMN total_cents BIGINT;
CREATE INDEX CONCURRENTLY invoices_tenant_created ON invoices (tenant_id, created_at);
""",
    "bash": """#!/usr/bin/env bash
# {name}
set -euo pipefail
cargo test --workspace -- --test-threads=4 2>&1 | tee /tmp/test.log
grep -c '^test .* ok$' /tmp/test.log
""",
    "yaml": """# {name}
jobs:
  test:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - run: make test
""",
}

COMMANDS = [
    ("cargo test --workspace", "running 214 tests\n" + "test api::orders::empty_cart_totals ... ok\n" * 6
     + "\ntest result: ok. 214 passed; 0 failed; 0 ignored"),
    ("git status --short", " M src/auth/refresh.rs\n M tests/refresh.rs\n?? notes.md"),
    ("npm run lint", "> lint\n> eslint src\n\n✔ No problems found"),
    ("pytest -q tests/test_backfill.py", "........                                     [100%]\n8 passed in 0.42s"),
    ("rg -n 'refresh_token' src", "src/auth/refresh.rs:41:    let token = store.refresh_token(user)?;\n"
     "src/auth/middleware.rs:88:        refresh_token: None,"),
    ("go test ./...", "ok  \tqueue\t0.214s\nok  \tqueue/retry\t1.020s"),
]

FILES = [
    "src/auth/refresh.rs", "src/api/orders.rs", "src/components/UsageChart.tsx",
    "pipeline/train.py", "migrations/0042_invoice_totals.sql", "internal/queue/retry.go",
    ".github/workflows/ci.yml", "src/lib.rs", "README.md",
]

SUBAGENT_TASKS = [
    ("Survey the auth callers", "Find every caller of refresh_token and report how each takes the lock."),
    ("Review the migration", "Review migrations/0042 for locking hazards on a 40M-row table."),
    ("Find flaky tests", "Run the e2e suite five times and list the tests that fail at least once."),
]

# The records harness tooling writes at the start of a conversation, before or
# between what the person typed. Each is (record fields, content). A record
# marked isMeta is never a user message; the rest look like one and must be
# recognised by their text.
NOISE = {
    "system-reminder": ({"isMeta": True},
                        "<system-reminder>\nThe user opened the file README.md in the IDE. This may or may "
                        "not be related to the current task.\n</system-reminder>"),
    "clear-command": ({}, "<command-name>/clear</command-name>\n            <command-message>clear</command-message>\n"
                          "            <command-args></command-args>"),
    "local-command-stdout": ({}, "<local-command-stdout></local-command-stdout>"),
    "caveat": ({"isMeta": True},
               "Caveat: The messages below were generated by the user while running local commands. "
               "DO NOT respond to these messages or otherwise consider them in your response unless the "
               "user explicitly asks you to."),
    "task-notification": ({"origin": {"kind": "task-notification"}},
                          "<task-notification>\n<task-id>b7f2k9</task-id>\n<tool-use-id>toolu_01Notify</tool-use-id>\n"
                          "<status>completed</status>\n<summary>Background command \"make\" completed "
                          "(exit code 0)</summary>\n</task-notification>"),
    "skill": ({"isMeta": True},
              "Base directory for this skill: /home/user/.claude/skills/solve-task\n\n# Solve task\n\n"
              "Follow these steps: read the issue, reproduce it, fix the shared cause, add a test."),
    "compact-summary": ({"isCompactSummary": True, "isVisibleInTranscriptOnly": True},
                        "This session is being continued from a previous conversation that ran out of "
                        "context. The conversation is summarized below:\n\nThe user asked to fix the token "
                        "refresh race. The read now happens inside the lock and a regression test covers it."),
}

AGENTS_MD = "# AGENTS.md instructions for {cwd}\n\n<INSTRUCTIONS>\n- Keep commits small.\n- Run the tests before you commit.\n</INSTRUCTIONS>"
ENVIRONMENT_CONTEXT = ("<environment_context>\n  <cwd>{cwd}</cwd>\n  <approval_policy>on-request</approval_policy>\n"
                       "  <sandbox_mode>workspace-write</sandbox_mode>\n  <shell>bash</shell>\n</environment_context>")


def code_block(stream, language=None):
    language = language or stream.choice(sorted(CODE))
    name = stream.choice(["apply_limit", "backfill_totals", "usage_series", "deliver_with_retry",
                          "invoice_totals", "run_suite", "ci_matrix"]) + f"_{stream.randint(1, 999)}"
    return f"```{language}\n{CODE[language].format(name=name)}```"


def long_reply(stream, paragraphs):
    """A reply with prose and fenced code in several languages."""
    parts = [stream.choice(REPLIES)]
    for _ in range(paragraphs):
        parts.append(code_block(stream))
        parts.append(stream.choice(REPLIES))
    return "\n\n".join(parts)
