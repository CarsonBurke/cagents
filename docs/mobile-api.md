# Mobile API

A phone app follows and steers a computer's agents through
`cagents daemon bridge --mobile`, run over SSH. The bridge speaks a stable,
versioned JSON-lines API on its standard input and output and translates it to
the daemon's internal protocol, which changes with every release. The message
definitions live in the `cagents-mobile-api` crate (`crates/mobile-api`); this
page describes the wire format a client implements.

API version 1 covers:

- the computer's sessions and their activity, including delegated agents;
- the person's unacknowledged notifications;
- following a native chat as it streams, and loading its earlier history;
- sending, steering or queueing a message, interrupting a turn, and answering
  approvals and questions.

Terminal-surface sessions are listed but can't be followed, and attachments,
full diffs and full tool output stay on the computer.

## Connecting

The phone opens an SSH connection and runs the bridge as the remote command.
Restrict the phone's key to that command in `~/.ssh/authorized_keys`, so the
key can do nothing else:

```text
command="/home/person/.local/bin/cagents daemon bridge --mobile",restrict ssh-ed25519 AAAA... phone
```

Use the absolute path of the installed `cagents`. `restrict` disables port,
agent and X11 forwarding and the terminal; the bridge needs none of them.

The bridge acts with the person's own authority, like the desktop app: anyone
holding the key can read the person's conversations, answer approvals and send
messages. It exposes only this API, never the daemon's internal protocol. It
refuses to run inside an agent's shell, and starts the daemon if it isn't
running.

The bridge exits, closing the connection, whenever its daemon stops or is
replaced; after an update, the next connection runs the updated bridge. A
client reconnects with backoff and watches again what it was showing.

Before the first line, a failure to start (no daemon could be started, or a
refused environment) is written to standard error, and the command exits
with a non-zero status without writing to standard output.

## Pairing

Rather than having the person copy the phone's key into `authorized_keys`, the
desktop app can pair a phone: its status bar's phone control shows a pairing
code as a QR code, and the person scans it with the app and confirms on the
computer.

The code is a URI:

```text
cagents-mobile://pair?v=1&name=workstation&user=person&port=22&hostkey=SHA256%3A…&host=workstation.tail1234.ts.net&host=100.101.102.103&host=192.168.1.20&key=…&expires=1800000300
```

The query is form-encoded (`application/x-www-form-urlencoded`), as Rust's
`url` crate writes it: `+` stands for a space and `%2B` for a plus, so decode
it as a form, not with plain percent-decoding. A `hostkey` such as
`SHA256:ab+c/d` arrives as `SHA256%3Aab%2Bc%2Fd`. Parameters may come in any
order; `host` keeps the order it is given in.

| Parameter | Meaning |
| --- | --- |
| `v` | The code's format, `1`. A client refuses a version it doesn't know. |
| `name` | The computer's host name, to show. |
| `user`, `port` | The SSH login and port. |
| `hostkey` | The SHA-256 fingerprint of the computer's Ed25519 host key, as `ssh-keygen -lf` prints it. Pin it for each `host`, rather than trusting a key on first use, and negotiate `ssh-ed25519` host keys so the fingerprint applies. The computer shows no code without it. |
| `host` | Repeated: the addresses to try, in order. The Tailscale name and address come first, then local network addresses. Addresses where the computer's firewall (ufw) blocks SSH are left out. |
| `key` | A one-time Ed25519 private key: its 32-byte seed, base64url without padding. |
| `expires` | When the code stops working, in Unix seconds. |

The one-time key is authorized until `expires`, with `restrict`, to run only
`cagents daemon bridge --pair DIRECTORY`; sshd refuses it after its
`expiry-time`, written in UTC, even if the computer never removes it. That
needs OpenSSH 8.7 or later on the computer. Connect with it, and send one
line:

```json
{"type":"pair","key":"ssh-ed25519 AAAA…","name":"Pixel 9"}
```

`key` is the phone's own public key, in OpenSSH form; only Ed25519 keys are
accepted. `name` is how the computer will list the phone. Once the request is
in front of the person, the computer answers:

```json
{"type":"pair_waiting"}
```

and then, when the person decides, one of these, and closes the connection.
The code must be used before it expires; the person then has five more
minutes to answer.

```json
{"type":"paired","host":"workstation"}
{"type":"pair_declined"}
{"type":"pair_error","message":"This pairing code has expired. Show a new one on the computer."}
```

`pair_error` may also come at once, instead of `pair_waiting`: an expired or
already used code, or a request it couldn't read. While it waits, show the
fingerprint of the phone's own key: the computer shows it beside the
question, so the person can tell their phone's request from one sent by
someone else who saw the code. Its message is written for
the person. After `paired`, connect again with the phone's own key and run the
bridge as described above: the computer has added

```text
command="…/current/bin/cagents daemon bridge --mobile",restrict ssh-ed25519 AAAA… Pixel 9
```

and removed the one-time key. Closing the connection before the answer
withdraws the request. The definitions are in
`cagents_mobile_api::pairing`.

## Framing

Each message is one UTF-8 JSON object on one line, ending with `\n`. A client
line may be at most 1 MiB; a longer one ends the connection. Blank lines are
ignored.

Every message has a `type`. The client sends `hello` and `request`; the server
sends `welcome`, `ok`, `error` and `event`.

## Handshake

The client's first line names the API versions it speaks:

```json
{"type":"hello","api_min":1,"api_max":1,"client":{"name":"cagents-android","version":"0.1.0"}}
```

The server chooses the newest version both speak:

```json
{"type":"welcome","api":1,"server_version":"0.42.0","host":"workstation"}
```

When they share none, the server answers with an `unsupported_api` error
(without an `id`) whose message says which side to update, and exits. Anything
other than `hello` first gets a `bad_request` error and the bridge exits.

## Requests

A request carries a client-chosen numeric `id`, an `op` and its `params`
object, which may be omitted when empty:

```json
{"type":"request","id":4,"op":"conversation.send","params":{"session":"0b5f…","text":"Run the tests too"}}
```

Each request is answered once, with `ok` or `error` carrying the same `id`:

```json
{"type":"ok","id":4}
{"type":"error","id":4,"error":{"code":"not_watching","message":"Open the conversation before acting on it."}}
```

The error `message` is written for the person and can be shown as is. The
`code` is for the client's logic:

| Code | Meaning |
| --- | --- |
| `bad_request` | The request was malformed or its parameters don't fit. |
| `unsupported_api` | No common API version. |
| `unknown_op` | This server doesn't know the op; it is older than the app. |
| `not_found` | The session or request no longer exists. |
| `not_watching` | The op needs `conversation.watch` first. |
| `not_watchable` | The session can't be followed: it isn't a running native chat. |
| `not_allowed` | The daemon refused the action. |
| `unavailable` | The daemon couldn't do it now; it may work later. |
| `internal` | Something failed on the computer. |

An `error` without an `id` answers a line that couldn't be read as a request.

Data arrives as events, not in replies. A watch's `ok` is sent before its
first event.

| Op | Params | Effect |
| --- | --- | --- |
| `ping` | | Answers `ok`. |
| `sessions.watch` | | Sends `sessions` now and whenever the list changes. |
| `sessions.unwatch` | | Stops those events. |
| `notifications.watch` | | Sends `notifications` now and whenever they change. |
| `notifications.unwatch` | | Stops those events. |
| `notification.acknowledge` | `id` | Dismisses a notification everywhere. |
| `conversation.watch` | `session` | Sends a reset `conversation`, then its changes. Watching again restarts it. |
| `conversation.unwatch` | `session` | Stops following it. |
| `conversation.load_earlier` | `session` | Extends the window to older items. |
| `conversation.send` | `session`, `text`, `mode` | Sends a message. `mode` is `send` (default), `steer` (into the running turn) or `queue` (after it). |
| `conversation.interrupt` | `session` | Stops the running turn. |
| `conversation.answer` | `session`, `request`, `action`, `answers` | Answers a pending request. |

The `conversation.load_earlier`, `send`, `interrupt` and `answer` ops need the
conversation to be watched. Several conversations can be watched at once.
A watched conversation counts as being looked at, so the computer keeps its
history current; unwatch it when the app goes to the background.

A request that a watch was still carrying out when the watch ended, such as
when it was unwatched or watched again, is answered with `unavailable`: the
computer may or may not have acted on it.

`conversation.answer` takes one of the request's `actions` by `id`, and for
each field the person filled, `{"field":"…","values":["…"]}`. A field left out
is answered with no values. Nothing is approved by default: the person
approves only by choosing an action.

## Events

```json
{"type":"event","event":"sessions","data":{"sessions":[…]}}
```

| Event | Data |
| --- | --- |
| `sessions` | The whole session list. |
| `notifications` | Every unacknowledged notice, newest first. |
| `conversation` | A change to a watched conversation. |
| `conversation_ended` | `session`, `message`: a watched conversation stopped being followable, such as when it was closed (noticed within two seconds). Watch again to retry. |
| `closing` | `message`: the bridge is about to exit, such as when the daemon stops. |

### Sessions

Each session has an `id`, its `lineage` (the conversation it continues),
`surface` (`chat` or `terminal`), `provider` and `provider_label`, `title`,
`project`, `directory`, `activity` (`idle`, `working`, `waiting`, or
`unknown`, as for one that has exited),
`exited`, `failure`, `active_at` (Unix milliseconds) and `watchable`. A
session started as an agent has `agent`, with its `owner` (`user`, `session`
or `automation`), the owning session's `owner_lineage`, and `run`, its run
state.

Sessions and notifications are checked every two seconds.

### Conversations

A client keeps a conversation's items by `id` and shows them in `order`:

```json
{"type":"event","event":"conversation","data":{
  "session":"0b5f…","reset":false,
  "state":{"activity":"working","busy":true,"ready":true,"exited":false,"has_earlier":false,"loading_earlier":false,"requests":[],"queued":[],"plan":[],"subagents":[]},
  "items":[],
  "appends":[{"id":"msg-7","offset":120,"text":" and the tests pass.","at":1795000000000}]}}
```

- `reset` replaces everything held: `items` is the whole window and `order`
  is present.
- `state` is sent whole every time.
- `order`, when present, is every item's ID in display order.
- `items` are new or changed items, each replacing what is held under its ID.
- `appends` add streamed text to held text items, in order. `offset` is the held text's
  length in UTF-16 code units, which is what a Kotlin or Java `String`
  counts. If a client's copy has a different length, it missed something and
  should watch the conversation again.

An item has a `role` (`user`, `assistant` or `system`), `started_at` and `at`
times, and a `kind` with its own fields: `text`, `command`, `changes`,
`web_search`, `tool`, `image` or `delivery`. Command and tool output is
clipped to its last 4 KiB and tool input to its first 2 KiB, with `clipped`
set. File changes carry the paths and line counts, not the diff.

On a slow connection, updates are combined: the bridge holds back while
the phone is behind and then sends everything that changed since the last
update it sent.

`state.requests` holds what the conversation waits for the person to decide.
Each request has a `kind` (`approval`, `question` or `consent`), a `title` and
`description`, its `fields` and its `actions`.

## Compatibility

The API only grows within a version:

- New ops, events, fields and enum values may be added. A field that is
  absent has its default: false, empty or none.
- Nothing is removed, renamed or given a new meaning. A change that would do
  that is a new API version, and the bridge keeps speaking older versions it
  still supports.

A client must therefore:

- ignore fields it doesn't know (in kotlinx.serialization,
  `Json { ignoreUnknownKeys = true }`);
- send only ops, parameters and values the server's `welcome` version
  defines, or handle `unknown_op` and `bad_request` from an older server;
- map an enum value it doesn't know to an unknown case rather than failing
  (an `Unknown` case with a custom serializer or `coerceInputValues`), and an
  item `kind` it doesn't know to a placeholder;
- ignore events it doesn't know;
- treat `unknown_op` as a feature this computer's cagents doesn't have yet.

`crates/mobile-api/fixtures/v1.jsonl` records an example of every message.
`tests/mobile_api_contract.rs` checks that each still decodes and says the
same, so a change that breaks an existing client fails. Record a new
example by adding it to the test and running it with
`CAGENTS_RECORD_MOBILE_API=1` (or append the lines it prints); never edit or
remove a recorded line.
