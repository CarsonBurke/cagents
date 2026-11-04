# Native chat provider parity

Native chat uses each provider's structured protocol. Resume preserves the
conversation UUID, account and project; it does not approximate a resume by
copying messages into a new conversation. The searchable resume picker also
reads bounded metadata from configured provider history without model calls.

The following describes the APIs examined for Claude Code 2.1.286, Codex
app-server 0.159.3, and Oh My Pi 18.2.9.

| Capability | Claude | Codex | OMP |
| --- | --- | --- | --- |
| Native and CLI history discovery, exact resume | Supported | Supported | Supported |
| Send, steer, queue, interrupt | Supported | Supported | Supported |
| Remove a queued message, or take it back into the composer to revise | Supported, with attachments | Supported, with local images | RPC unavailable |
| Start selected queue entry next | API unavailable | Supported | RPC unavailable |
| Queue entry shows its delivery | Next step or next turn | Next turn | Next turn |
| Branch conversation | No native branch control | Separate conversation, optionally after a completed turn | Before a user message; restores its draft |
| Child transcripts and lifecycle | Supported | Supported, including nested children | Supported |
| Child approvals | Provider permission requests | Explicit, labeled child requests | Provider input dialogs |
| Direct child messaging, stop and restart controls | No native controls | Remaining native UI gap | RPC unavailable |
| Native goals: objective, status, pause/resume/clear | API unavailable | Native Goal API where supported; commands and state panel | RPC unavailable |
| Suggested next prompt | Optional SDK suggestion | Optional experimental prediction API | API unavailable |
| Model, reasoning, attachments, skills, compaction | Supported where provider advertises them | Supported | Supported |
| Runtime permission policy | Provider modes; source-labelled rules and Auto context with live-session edits | Common policy presets | Launch/settings configuration |
| Permission recovery | Footer notice, denied input/reason, explicit retry drafts, pending approvals | Pending approvals | Pending approvals |
| Steering timing | Next delivery boundary | Active turn steering | Between tools or after the turn |

Claude async steering and queueing are enabled for 2.1.286 and newer. Messages
are written to the running provider immediately with `next` or `later`
priority. Queued input appears immediately in a bounded list above the composer;
provider lifecycle acknowledgments move it into the transcript only when consumed.
Revising an entry removes it from the provider and returns it to the composer
rather than editing it in place, on every provider that can remove one: the
provider cannot consume a message mid-edit. The text returns only on the
provider's own confirmation of the removal (Claude's `cancelled` or `discarded`
lifecycle state, Codex's `thread/queue/delete` result), since an entry leaving
the queue is also what delivery looks like. Codex's in-place
`thread/queue/update` is therefore not used.
Background child work does not keep the parent composer busy after its result.
Older versions retain local follow-up delivery after the current turn: each
follow-up is its own queue entry, and they are sent together as one prompt.

Codex retains additional typed APIs without corresponding native UI controls:
queue reordering, review, plan/collaboration modes and background terminal
management. Arbitrary TUI slash commands have no equivalent app-server
dispatcher. OMP's extension-specific terminal interfaces and full session tree
also remain provider-specific surfaces. These distinctions are not represented
as working native buttons.

Native Goal integration uses the Codex 0.160.0-generated app-server contract:
`thread/goal/get`, `thread/goal/set`, `thread/goal/clear` and goal notifications.
This API contract is distinct from durable-continuity qualification. Missing or
unsupported Goal reads never establish that an older retained thread is safe
to resume. See [native goals](user-guide.md#native-goals) for commands and
recovery behavior.

Codex owner recovery requires positive evidence: reconnect to the retained
owner, or resume after confirmed close or a kernel boot change. Uncertain
same-boot ownership refuses to start another writer. The daemon-death durability
allowlist is 0.157.1, 0.160.1 and 0.161.0, each qualified through a killed daemon as
[the daemon guide](daemon.md#native-provider-continuity) describes; 0.159.3 was
tested only for connection loss against a local mock API, which does not
establish that guarantee, so it is not listed.

Validation uses recorded protocol fixtures, fake provider executables in an
isolated native UI session, and a real Codex executable against a local mock
Responses API. None requires a paid model prompt.

Protocol references: [Codex app-server documentation](https://learn.chatgpt.com/docs/app-server),
[OMP 18.2.9 RPC types](https://unpkg.com/@oh-my-pi/pi-coding-agent@18.2.9/src/modes/rpc/rpc-types.ts),
[OMP child RPC implementation](https://unpkg.com/@oh-my-pi/pi-coding-agent@18.2.9/src/modes/rpc/rpc-subagents.ts),
and [Claude Agent SDK streaming input](https://platform.claude.com/docs/en/agent-sdk/streaming-vs-single-mode).
Installed Codex-generated JSON schemas and the installed Claude SDK type
declarations were also checked for the exact wire fields used by the adapters.

## Suggested next prompts

When the provider offers a suggested next prompt, native chat displays it in
an empty composer with a Tab hint. Tab places the whole suggestion in the
composer; Enter sends it only after acceptance. Typing dismisses the suggestion
for that turn, including suggestions that arrive after the draft was typed and
cleared. Suggestions never replace a draft, enter the transcript, or survive a
provider restart. A key pressed against an older suggestion cannot accept a
replacement suggestion or overwrite newer input.

Claude is opted in through the SDK initialize request's `promptSuggestions`
field. An older provider that rejects that optional field receives one retry of
the same handshake without it. Provider settings and
`CLAUDE_CODE_ENABLE_PROMPT_SUGGESTION=false` continue to control delivery.
The [Claude documentation](https://code.claude.com/docs/en/interactive-mode#prompt-suggestions)
explains that suggestions may be skipped and can consume provider usage.
The official [Claude Agent SDK 0.3.296 declaration](https://unpkg.com/@anthropic-ai/claude-agent-sdk@0.3.296/sdk.d.ts) describes at most one
`prompt_suggestion` following an eligible `result`, carrying `session_id`,
`suggestion`, and `uuid`. Claude supplies no source-turn ID, so its adapter
relies on that ordered stream contract and accepts only the current completed
idle generation in the exact session. New input or a new turn invalidates it.

Codex uses the optional experimental
[`thread/prediction/request` and `thread/prediction/updated` API](https://github.com/openai/codex/blob/4bc83aa5f7ebfec8492bb5747affe0fc8433fcad/codex-rs/app-server-protocol/src/protocol/v2/thread_prediction.rs).
Requests name the exact completed source turn. Notifications from another
session or turn are ignored, and no more than one optional prediction request
is retained at a time. Missing methods, prediction failures, and empty results
are quiet and do not affect the conversation. An older Codex server without
this experimental method continues to work without suggestions. Prediction
requests are made only for the main conversation, never for observed subagents.
