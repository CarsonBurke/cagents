# Frozen protocol-127 inspection frames

These synthetic golden frames are independently encoded from the positional
declarations at `c07b6f78e906871b0e8ef98e318b27b0b86c268b`, whose IPC version is
literal **127**. They were not captured from a running protocol-127 service and
do not establish provider, process ownership, native UI, or migration runtime
validation. All identities, paths, and text are public fixture data.

`generate.py` uses only Python's standard library and explicit frozen field/tag
encoding. It does not import a current Rust DTO, invoke Cargo, run a product
binary, connect to a daemon, or start a provider. The expected byte arrays must
remain independent of the implementation being tested.

```sh
python3 tests/fixtures/upgrade-inspection-v127/generate.py --check
```

Running without `--check` reproduces the committed `.bin` files. Each frame has
a four-byte big-endian payload length followed by postcard data. Unsigned
integers and enum tags use unsigned LEB128. Strings/bytes/sequences carry a
length; structs concatenate fields; options encode `0` or `1` plus the value.
The transparent UUID-based `TerminalId` uses `Uuid::serialize_bytes`, so it is
**length 16 followed by 16 raw UUID bytes**, not a string or an unprefixed array.
Paths here serialize as their UTF-8 strings. `Arc<T>` serializes `T` directly.

## Provenance and fixed data

The frozen definitions are `src/terminal/ipc.rs` (`ClientEnvelope`,
`ClientRequest`, `ServerEnvelope`, `ServerMessage`, `ServerResponse`, and the
three-field `ProtocolError`), `src/model.rs` (`TerminalId`, `HostSessionProof`,
`LaunchRecipe`), `src/agent.rs` (`Provider`), and `src/chat.rs` (`Launch`,
`Session`, `NativeContinuity`, `Snapshot`, and its nested values).

The current source-service proof is `(source, boot, actor)` in the serialized
order `(host_session_id, boot_id, host_incarnation)`. The original saved proof
is `(saved, boot, previous)`. This fixture's use of the current proof in a copied
history attachment request changes neither the saved source nor its provider
execution authority.

The saved chat ID is `00000000-0000-4000-8000-000000000127`; its provider session
is `11111111-1111-4111-8111-111111111111`. The retained chat ID is
`00000000-0000-4000-8000-000000000128`; its provider session is
`22222222-2222-4222-8222-222222222222`. Both use this launch recipe:

```text
provider = Claude (enum tag 1)
program = claude
arguments = [--model, fixture]
working_directory = /fixture/removed-worktree
environment = {CLAUDE_CONFIG_DIR: /fixture/claude-config}
unset_environment = {FIXTURE_UNSET}
terminal = None
owned_codex_listener = None
```

The original saved session has the saved proof and `ReconnectOnly` (tag 0).
The retained session has the current proof and
`RetainedClaudeActor { protocol: 2 }` (tag 2, then integer 2). `ChatListed`
contains both original canonical sessions, in saved/retained order. Resolve
returns the retained session unchanged. Attach contains a copied saved session
whose proof is current, with every other field unchanged.

## Request/reply sequence

Connect has no envelope proof; all later requests have the current proof.
Every server response has that current proof and its exact request ID.

| ID | Request file | Request tag | Response file | Response tag |
| --- | --- | --- | --- | --- |
| 1 | `request-01-connect.bin` | Connect 0, expected current proof, no resume | `response-01-connected.bin` | Connected 0, current proof, no resumed terminals |
| 2 | `request-02-chat-list.bin` | ChatList 29 | `response-02-chat-listed.bin` | ChatListed 13, both sessions |
| 3 | `request-03-list.bin` | List 1 | `response-03-listed.bin` | Listed 1, no terminals |
| 4 | `request-04-resolve.bin` | ResolveUpgradeChat 38, exact retained source | `response-04-resolved.bin` | UpgradeChatResolved 18, exact retained source |
| 5 | `request-05-chat-attach.bin` | ChatAttach 22, copied saved source/current proof | `response-05-chat-attached.bin` | ChatAttached 11, exact requested session/loading snapshot |
| 6 | `request-06-chat-visibility.bin` | ChatVisibility 25, saved ID, visible true | `response-06-accepted.bin` | Accepted 5 |
| 7 | `request-07-chat-read.bin` | ChatRead 26, saved ID, after generation None | `response-07-chat-snapshot.bin` | ChatSnapshot 12, saved ID, Some settled snapshot |
| 8 | `request-08-chat-detach.bin` | ChatDetach 23, saved ID | `response-08-detached.bin` | Detached 4 |

`event-chat-changed.bin` has `request_id = None`, the current proof,
`ServerMessage::ChatChanged` tag 7, and the exact saved chat ID. No fixture
authorizes `ChatStart`, `ChatClose`, `ChatRelease`, provider input, or an
execution environment.

## Frozen snapshot schema

The Rust `notification_preview` field is `#[serde(skip)]` and occupies **no**
wire position. All `#[serde(default)]` fields below still serialize. The 42
serialized fields are, in order:

```text
sampled_at, generation, turn_generation, finished_at,
items, text_copy_sources, requests, capabilities, models,
selected_model, reasoning, fast_mode, permission_mode, permissions,
permission_denials, commands, skills, usage, busy, detail, session_id,
native_continuity, title, error, read_only_reason, exited,
has_earlier, has_later, composer, prompt_suggestion, open_url, waiting,
plan, subagents, ready, queued_messages, queue_removals, history_loading,
interrupt_mode, branch_points, forked, file_changes
```

Both snapshots use the saved provider session ID, `ReconnectOnly`, `exited =
true`, and this exact read-only reason:

```text
Saved conversation history is read-only; no provider was started.
```

The Attach snapshot has `history_loading = true`, `ready = false`, and no items.
The Read snapshot has `history_loading = false`, `ready = true`, and one item:
`started_at = None`, ID `frozen127-history`, `Role::Assistant` tag 1,
`ItemContent::Text` tag 0 with text `frozen127 retained history`, `thinking =
false`, `Phase::Final` tag 0, and `at = None`. Item order is
`(started_at, id, role, content, at)`.

Other snapshot values are their frozen defaults: numeric zero, empty
strings/collections, false booleans, and absent options. In particular:

```text
TextCopySources = (conversation_revision=0, conversation=[], children={})
Capabilities = (steer=false, queue=false, attachments=false, compact=false,
 model_selection=false, reasoning_selection=false, fast_mode=false,
 permission_modes=[], skill_reach=Anywhere(tag 0), queue_remove=false,
 queue_start=false, interrupt_modes=[], branch=false, stop_subagent=false,
 interrupt_stops_background=false)
Usage = (input_tokens=0, output_tokens=0, context_tokens=0, context_limit=0,
 cost_microusd=None, cost_estimate=None)
FileChangesSummary = (request_id=0, revision=0, files=[], total_files=0,
 applied_files=0, has_more=false, coverage=(false,false,false,false))
```

Coverage boolean order is `has_unloaded_history`, `has_imported_history`,
`has_incomplete_delegated_history`, `has_uncached_changes`.

## Legacy error cross-check

`error-permission-denied.bin` correlates request 5 and has the current proof.
Its `ServerMessage::Error` tag is 6. The frozen `ProtocolError` consists of
**exactly** `(code = PermissionDenied(tag 10), message = "legacy source
refused", retryable = false)`. There is no `details` field or trailing absent
option byte. New protocol-139 error serialization must not generate this
fixture.

`error-minimal.bin` uses proof `(s,b,i)` and has this hand-authored complete
frame, independently cross-checked by the generator:

```text
00000022 7f 01 05 0173 0162 0169 06 0a 15
6c656761637920736f757263652072656675736564 00
```

It spells payload length 34, version 127, request `Some(5)`, three one-byte
proof strings, Error 6, PermissionDenied 10, message length 21, the UTF-8
message, and false. Such an old error must remain a refusal; inability to decode
it is never permission to retry as a user, start a provider, or accept a source.
