# Offline Claude pre-initialize contract

This fixed provider fixture extends native-validation preparation for PR #1859 at `c46c68306197bbfc7989db4bc4ff67201f3bb316`. It does not change production protocol, launcher, runtime or first-send behavior. Source only: the fixture, runtime tests and native interactions have **not been run** by this authoring task.

Select one scenario at process startup, before any protocol input:

```text
tests/fixtures/claude-preinit-contract.py --fixture-scenario SCENARIO [known generated Claude flags]
```

`--fixture-scenario=SCENARIO` also works. There is no default, prompt-selected scenario, environment selection or arbitrary output payload. `--version` alone, or with the fixed scenario prefix, prints `2.1.286 (offline preinit-contract fixture)` without reading stdin. A real launcher supplies its generated `--session-id` UUID; direct offline wiring otherwise uses a fixed fixture UUID. `--resume` is accepted as an argument only; this script never reads history. Native wiring should start a fresh conversation so the *production* adapter does not attempt saved-history access before this fixture runs.

| Scenario | Fixed sequence | Expected current-source runtime outcome |
| --- | --- | --- |
| `healthy-advisory` | Match/ACK initialize; accept one real user frame; send system init, future top-level/system/stream advisories, one non-JSON line and a fixed text answer/result. Keep stdin/stdout open afterwards. | Initialization completes, fixed answer appears, no advisory/private payload is rendered and the conversation remains usable until owned close. |
| `preinit-advisory-pending` | Receive initialize; send `keep_alive` and `future_advisory`; **never ACK**. Block reading the original stdin with stdout open. | Real runtime's unchanged 30-second startup deadline reports `Provider initialization timed out before handshake completion`; advisory receipt must not end the deadline. |
| `preinit-eof` | Receive initialize; return exit0 immediately, without ACK or output. | Unrequested clean EOF is an error: `Provider exited before completing initialization`. No ready/success claim. |
| `preinit-noise-eight` | Receive initialize; emit exactly eight fixed non-JSON lines, no ACK; leave pipes/process pending. | `Provider printed 8 consecutive invalid protocol lines` with a bounded escaped preview; owned cleanup must retain the original failure. |
| `healthy-noise-reset` | Before ACK: seven malformed lines, valid keep-alive, seven malformed lines, valid advisory, matching ACK. Then the same one-turn healthy answer (including one isolated non-JSON line). | Each valid JSON frame resets the actual consecutive-noise counter. Fourteen cumulative lines are not eight consecutive failures. Initialization and the answer succeed. |

The runtime owns elapsed time and cleanup. The fixture has no timer, sleep, deadline override, signal handler, subprocess, network, filesystem/history/config/account discovery, or environment reads. Pending scenarios perform bounded blocking stdin reads; they exit on original stdin EOF or remain pending for real runtime teardown. If the native observer closes/cancels first, that is a requested-close control, **not** an observed deadline. No timeout/status/settlement receipt is synthesized.

Input is bounded to 64 KiB per complete JSON line and 32 frames, with duplicate JSON members refused. One initialize request and at most one user turn are supported. Other well-formed control requests receive a fixed unsupported error. Invalid input/arguments or a pipe error produce one fixed bounded stderr line and exit2, without echoing any supplied text. The fixture never echoes the user prompt; the answer is fixed public data. The malformed stdout in two scenarios is intentional protocol test data.

The supported generated flags are `--print`/`-p`, `--verbose`, `--include-partial-messages`, `--replay-user-messages`, `--input-format=stream-json`, `--output-format=stream-json`, `--permission-prompt-tool=stdio`, one UUID `--session-id` or `--resume`/`-r`, `--permission-mode`, plus validated/ignored `--model`, `--agent`, `--effort`, `--setting-sources` and the listed inert boolean CLI options in the source. Missing, unknown or repeated flags refuse; no config/tool/plugin path is opened. If future wiring produces unsupported arguments, change the separately reviewed fixture contract rather than silently ignoring them.

**Native entrypoint wiring remains pending.** The production Claude argument allowlist rejects `--fixture-scenario` in `LaunchRecipe.arguments`. Keep that allowlist unchanged. A later source-pinned private entrypoint may supply the fixed fixture scenario outside those arguments and forward the actual generated Claude suffix. This script itself launches no entrypoint or child. Standalone source compatibility is not actual launcher or native qualification.

Existing source references at the base revision:

- `src/chat/protocol/claude.rs`: `arguments`, `initialization_pending`, `initialize`, and `initialization_waits_for_the_matching_success_including_optional_retry`.
- `src/chat/runtime.rs`: unchanged `IO_TIMEOUT` and JSON-lines/EOF/startup branches; owning HOST parents `claude_advisories_do_not_disable_the_real_pipe_initialization_deadline`, `claude_clean_pipe_eof_requires_initialization_but_requested_close_does_not`, `json_line_noise_and_unknown_claude_frames_finish_through_the_real_pipe`, `json_line_noise_resets_its_failure_streak_after_each_valid_frame`, `json_line_noise_fails_after_eight_lines_with_a_bounded_escaped_preview`.
- The existing `claude-unknown-frames.py` always ACKs initialization first and remains byte-identical.

Later validation must use the actual development runner, matched isolated executables, original fixture/provider process waits and positive family cleanup. Capture initial pending/error and healthy completed-answer states at matched revision, viewport/scale/theme/prompt, including narrow layout, visible failure/recovery/close controls and responsiveness. Label these as offline fixture evidence; preserve failed outcomes and actual 30-second elapsed observations without claiming live Claude/auth/tool/history acceptance. Root owns native/controller wiring, Before/After selection, screenshots and independent UX review. These sources establish neither startup custody nor native first-send correctness.


## Fixed offline entrypoints

These five source-bound entrypoints close only the fixture argument-routing gap.
Each runs the exact sibling `claude-preinit-contract.py` in the same Python
process, using its existing `__main__` error/exit handling. It prepends the one
literal scenario below and forwards the launcher's original argument suffix
unchanged. It does not spawn a child, modify environment variables, or read any
account, home, configuration or history. Loading the fixed sibling source is
part of this entrypoint; the fixture itself continues to use stdin/stdout only.

| Fixed executable under `tests/fixtures/` | Literal scenario |
| --- | --- |
| `claude-preinit-healthy-advisory.py` | `healthy-advisory` |
| `claude-preinit-advisory-pending.py` | `preinit-advisory-pending` |
| `claude-preinit-eof.py` | `preinit-eof` |
| `claude-preinit-noise-eight.py` | `preinit-noise-eight` |
| `claude-preinit-noise-reset.py` | `healthy-noise-reset` |

A later reviewed offline launch can select one fixed absolute entrypoint as its
program, while its `LaunchRecipe.arguments` contains only the ordinary supported
Claude arguments. The adapter then supplies its normal generated stream-json
suffix and fresh session identity. Do **not** place `--fixture-scenario` in the
recipe: the production allowlist continues to reject it. There is no generic
mode, path, command or scenario parameter on an entrypoint; supplying another
scenario in the forwarded suffix is rejected as a duplicate by the original
fixture. `--version` is forwarded in the same way and uses the fixture's existing
version-only path without reading stdin. No executable has been invoked here.

The original helper review pinned SHA256
`9ffa88df040ebdd7a7c548cdda9629977a9661c808e9e50a0d3b81730f284ec4`.
This native-hook successor changes only the executable header in the helper
and five fixed wrappers; all Python body bytes remain exact to that review.
Future native source admission must pin each selected entrypoint **and** this
sibling helper at their actual absolute paths, refuse changed or substituted
sources, and retain the original generated argv. This source packet does not
install anything, modify PATH/configuration, select an account, or establish
application launch authority. Python runtime/import context is a separate
execution-admission obligation, not an audited dependency closure here.

These new leaves are prepared against main-composed
`82a7bada70945fb3bc292d2a08e9766fbae73649` (tree
`6ed27cc2e3544c62548ea02fc0f6c79c9009b53d`); the earlier helper review at
`c46c68306197bbfc7989db4bc4ff67201f3bb316` remains unchanged source evidence.
The initial section's pending-entrypoint note describes the historical helper
packet. Fixed source entrypoints now exist; actual native integration, private
launch context, runner/controller wiring, compiled checks, process-custody
admission, live execution, screenshots and UX review are all still **UNRUN**.
Only one fixed synthetic user turn is supported, not general conversation,
authentication, tools, queued-message delivery or live Claude behavior.


## Isolated interpreter entry

All six executable Python files now start with `#!/usr/bin/python3 -ISB`.
The native recipe executes the fixed absolute wrapper directly, so the
interpreter receives the single combined `-ISB` option before the script.
`-I` excludes user and script search paths and ignores Python environment
routing; `-S` suppresses automatic `site` startup and customization;
`-B` prevents bytecode writes. Parent environment and HOME are not rewritten.
The wrappers still load only their fixed sibling in the same process.

The combined option is source-supported by CPython 3.13.0's
[short-option parser](https://raw.githubusercontent.com/python/cpython/v3.13.0/Python/getopt.c),
which includes these three flags and advances within one option argument.
The [Python 3.13 command-line reference](https://docs.python.org/3.13/using/cmdline.html)
describes their startup behavior. This is source evidence, not provenance or
execution evidence for the future selected `/usr/bin/python3`; its exact
interpreter/library binding and actual invocation remain pending.
Directly invoking an arbitrary interpreter without these flags is outside
this fixed entrypoint contract.

The development-only native hook uses exactly the five scenarios above,
selected by `CAGENTS_DEVELOPMENT_PREINIT_FIXTURE`. It requires private owned
0700 roots `/tmp/cagents-1859-native-<scenario>-<wide|narrow>-<before|after>`,
with fixed `data`, `config`, `cache`, `state`, `runtime` XDG children and
`CLAUDE_CONFIG_DIR` pointing at its `claude` child. The owning future runner
creates the empty namespace; bootstrap creates a one-use marker and fresh
synthetic project/config directories, without deletion or reuse.

The actual native composer displays the fixed public prompt, “Show the
offline initialization control.” Send, initialization, error, retry and
retirement use the normal reducer/client/runtime. Cursor/selection validation
retains the real editor state; slash, history, file, account and provider
escapes are not fixture inputs. Pending-close Escape and application-error
notices retain their existing controls. No Ready, completion, deadline,
process exit or cleanup success is manufactured. All native interaction,
interpreter execution, matched builds, screenshots and UX review remain
UNRUN; the fixed suffix does not establish a tested revision or permission.
