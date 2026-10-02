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
| Remove provider-confirmed queued message | Supported | Supported | RPC unavailable |
| Edit plain text queue entry, start selected entry next | API unavailable | Supported | RPC unavailable |
| Branch conversation | No native branch control | Separate conversation, optionally after a completed turn | Before a user message; restores its draft |
| Child transcripts and lifecycle | Supported | Supported, including nested children | Supported |
| Child approvals | Provider permission requests | Explicit, labeled child requests | Provider input dialogs |
| Direct child messaging, stop and restart controls | No native controls | Remaining native UI gap | RPC unavailable |
| Model, reasoning, attachments, skills, compaction | Supported where provider advertises them | Supported | Supported |
| Runtime permission policy | Provider modes; source-labelled rules and Auto context with live-session edits | Common policy presets | Launch/settings configuration |
| Permission recovery | Footer notice, denied input/reason, explicit retry drafts, pending approvals | Pending approvals | Pending approvals |
| Steering timing | Next delivery boundary | Active turn steering | Between tools or after the turn |

Claude async steering and queueing are enabled for 2.1.286 and newer. Messages
are written to the running provider immediately with `next` or `later`
priority. Provider lifecycle acknowledgments determine the queued preview;
background child work does not keep the parent composer busy after its result.
Older versions retain local follow-up delivery after the current turn.

Codex retains additional typed APIs without corresponding native UI controls:
queue reordering, review, plan/collaboration modes, goals and background terminal
management. Arbitrary TUI slash commands have no equivalent app-server
dispatcher. OMP's extension-specific terminal interfaces and full session tree
also remain provider-specific surfaces. These distinctions are not represented
as working native buttons.

Codex owner recovery requires positive evidence: reconnect to the retained
owner, or resume after confirmed close or a kernel boot change. Uncertain
same-boot ownership refuses to start another writer. The daemon-death durability
allowlist remains 0.157.1; testing 0.159.3 connection loss against a local mock API
does not establish the stronger killed-daemon guarantee.

Validation uses recorded protocol fixtures, fake provider executables in an
isolated native UI session, and a real Codex executable against a local mock
Responses API. None requires a paid model prompt.

Protocol references: [Codex app-server documentation](https://learn.chatgpt.com/docs/app-server),
[OMP 18.2.9 RPC types](https://unpkg.com/@oh-my-pi/pi-coding-agent@18.2.9/src/modes/rpc/rpc-types.ts),
[OMP child RPC implementation](https://unpkg.com/@oh-my-pi/pi-coding-agent@18.2.9/src/modes/rpc/rpc-subagents.ts),
and [Claude Agent SDK streaming input](https://platform.claude.com/docs/en/agent-sdk/streaming-vs-single-mode).
Installed Codex-generated JSON schemas and the installed Claude SDK type
declarations were also checked for the exact wire fields used by the adapters.
