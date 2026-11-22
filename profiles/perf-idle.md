# Idle UI profile

## Scope and measurement

The user-visible target is a focused window with 1 or 20 open, settled
chats/terminals: no application work should be scheduled merely to discover
that nothing changed.  The realistic follow-up workload is 20 chats with one
streaming and the other 19 idle, with the redact repository present for Git.

Native measurements were taken on fw16 by the workstation runner for 60
seconds, reading every `/proc/<ui-pid>/task/*/stat` and the UI trace.  The
saved raw records are under
`~/.local/state/cev/i3591c/idle/{before,after}-{1,20}.txt` on the runner.  At
the deadline-model head `fcdd00c4` (not this unmeasured corrective head):

| Workload | main wakeups/s -> deadline-model wakeups/s | CPU core % -> | Trace consequence |
| --- | ---: | ---: | --- |
| 1 settled terminal | 18.75 -> 10.57 | 0.394 -> 0.271 | 6 `RefreshUi(Tick)` messages in 60 s remained |
| 20 settled terminals | 13.08 -> 5.10 | 0.222 -> 0.181 | 6 `RefreshUi(Tick)` messages in 60 s remained |

RSS is not a growth measurement: it was sampled only at the start and end of
those same 60-second runs and moved with allocator reclamation (1-terminal
after: 167,508 KiB to 150,992 KiB; 20-terminal after: 116,484 KiB to
116,448 KiB).  There is no evidence yet for sustained RSS growth, nor a
cold-start-to-first-frame measurement.  The workstation script below must
repeat the idle runs on the final commit and add a 20-chat/one-streaming run,
first-frame timestamp, and 10-minute RSS samples before this profile can make
an after claim.

Patterns seen: fixed polling causes each timer to send a top-level `Message`,
which performs an app update and can rebuild the whole view.  The current
deadline conversion in `refresh_ui` removes its own settled-chat wake; it is
not enough while the independent clocks below stay armed.

## App subscription inventory

This is the complete audit of periodic UI subscriptions from `app.rs`
`subscription()` and its called app modules.  Streams listed as event-driven
are idle while their sender is quiet; they are not timer wakeups.

| Source | Current rate and condition | Focus | Work caused | What it needs |
| --- | --- | --- | --- | --- |
| `refresh_ui` reconciliation | 10 s while visible repository/PR/source-control work, blocked shared view, terminal handoff, or stale document exists | focused only | `RefreshUi`: full app update; local Git/source/project work | deadlines for retry, otherwise daemon/fs completion events |
| `refresh_ui` project rows | selected 60 s, open-PR 120 s, inactive 600 s; failed reads exponential to 30 min | focused only | one project read, then full result update | GitHub/daemon push plus stored rate-limit deadline as fallback |
| `git_ui` fallback | 60 s after partial watch/read failure; watcher retry 10 s | focused through refresh | local Git scan / watcher retry | filesystem watch event; one retry deadline only after failure |
| review requests | 10 s focused, 120 s unfocused; 30 s when daemon unavailable and focused | both | monitor scan, shared-cache/GitHub request when due | daemon shared scan revision; backoff deadline only if event source unavailable |
| account usage | 30 s whenever default status bar/Accounts/automatic account is visible | both | top-level account tick; focused window may probe providers | account cache expiry deadline; widget-local relative-time redraw |
| account sign-in | 1 s while CLI login is waiting | both | top-level login-state check | process exit/pidfd event; a timeout deadline only for error recovery |
| update install watch | 10 s in an installed focused build | focused only | filesystem/install inspection | installation file watch or stored mtime deadline |
| update release check | 6 h normally; exponential failures | both | GitHub update request | stored next-check deadline; one scheduler participant |
| update progress | 1 s while updating/retrying | both | installer poll/status update | child-process/protocol events; deadline only while retry is owed |
| Automation pane | 5 s while visible and focused | focused only | automation refresh and full app update | daemon revision stream; deadline only after a failed request |
| Work pane | 30 s while visible and focused, plus daemon watch | focused only | work-board refresh and relative labels | daemon watch for data; widget-local clock/redraw for labels |
| automation approvals | 2 s doubling to 16 s only while a focused window owes a failed read | focused only | approval daemon read | retain the bounded retry as a stored deadline |
| busy chat clock | 1 s if a visible chat is busy | visible chat | top-level chat tick for elapsed labels | widget-level `request_redraw_at`; no app message or full view |
| deferred thumbnails | 2 s while any target has a deferred thumbnail | target-specific | retry image decode / message | decode completion or per-item bounded backoff deadline |
| browser progress | 50 ms while a visible embedded browser loads (browser build) | visible card | full app progress tick | browser progress/load events, with widget-local animation if necessary |
| notice expiry | one-shot earliest expiry only | only while countdown active | notice retirement | already correct one-shot deadline |
| workspace/pane animation | renderer frame subscription only while animation is active | visible animation | frame redraw | already event/frame scoped |
| terminal/chat/workspace/notification/daemon/LSP/settings/document/Git streams | receiver/watch driven | varies | top-level event update | event driven; no periodic wake while silent |
| pull-request conversation, repository document, worktree-operation streams | mounted/in-flight only | scoped to consumer | top-level result/progress update | event driven; remove on unmount/completion |

One-shot task delays (repository language 150 ms, project close/session waits
50 ms, search debounce, confirmation expiry) are completion/retry deadlines,
not permanent subscriptions.  They should remain bounded tasks but publish a
deadline if they need coordinated wakeups.  Browser runtime's internal
one-millisecond event loop and LSP/session worker maintenance are not
application `Subscription`s; they need separate process/thread accounting in
the native profile.

## Systemic change

Replace the independent `time::every` subscriptions with one app scheduler.
Each source owns durable state and publishes `next_due() -> Option<(Instant,
generation)>` when a state transition creates, changes, or clears work.  The
app takes the minimum and installs exactly one `Subscription::run_with` sleep
keyed by that fixed deadline plus generation.  When it fires, it first removes
or advances every due source's stored deadline, then dispatches only those
source actions.  Completion, daemon revision, filesystem watch, process exit,
and browser events update state directly and re-publish a deadline only when
they leave retry/expiry work owed.  A past deadline must be actionable or be
removed before subscription construction.

The scheduler must not calculate a fresh `now + interval` in
`subscription()`, take per-project locks, allocate host keys, or scan all
cards.  State transitions calculate it once; subscription construction only
reads the already-minimized deadline.  A source can use a fixed monotonic
conversion for wall-clock API backoff, as `refresh_ui` now does.

Elapsed labels and spinners are separate: their widgets request their own
redraw at the next visible boundary, while app state remains untouched.  A
visible streaming chat keeps its existing data-event stream; it must not make
the 19 idle chats update or view again.  The scheduler must coalesce daemon
polls by repository/account/update key and share the result revision across
windows, rather than every pane issuing an identical request.

The largest removal is replacing review-request, account, update,
automation, and work fixed clocks with deadline/event publishers: in a
typical focused GitHub workspace those clocks dominate after `refresh_ui` is
quiet.  It requires a small `SchedulerState`, migration of each source's
existing retry/expiry state to monotonic deadlines, widget redraw APIs for
labels, and trace counters for scheduler fires, app updates, and views.

## Required native rerun

Run `~/.local/state/cev-steps/perf-idle-3591.sh` from the workstation with
`--remote fw16`, comparing `origin/main` to this PR's final SHA.  It must run
1 and 20 settled sessions for 60 seconds each, save per-thread `/proc` CPU
and context-switch deltas plus `CAGENTS_UI_TRACE`, and record RSS.  Extend the
same runner with the 20-chat/one-streaming/redact-repo fixture, a cold-start
first-frame marker, and ten one-minute RSS samples.  The final SHA cannot be
called a measured win until those after records exist.
