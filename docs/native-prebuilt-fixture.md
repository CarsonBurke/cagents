# Fixed a48 native qualification carrier

This is a developer qualification mode for the existing browserless a48
package, not an installer or a production recovery command. It uses the
person-started native fixture broker's existing User admission, request owner
and input/capture protocol. Ordinary checkout/build mode is unchanged.

The supported path is deliberately fixed:

- Candidate source: `a48d0f75eca3e48eac42bcb2955df53b2ec6ab9f`.
- Existing extracted package:
  `/var/tmp/coord-b9-a48-isolated-8a_xjx4x/package`.
- Archive provenance: `cagents-browserless-0.1.3-x86_64-unknown-linux-gnu.tar.gz`,
  SHA256 `592877f8c45d998e4046260149a0e46c5c988e0e74b47cebf6fcad27e14ba8f4`.
- The carrier checks all three executable digests/sizes/modes and the lease
  marker. Identical-content inode replacement is still refused. The offline
  Claude script is the unchanged `tests/fixtures/chat-composer-claude.sh`,
  SHA256 `a7c31648b41089d6b0919b4245ce1eef4b896ff932ab8cd41de9859fa4ce29f6`.

There is no request option for another executable, package, cwd, environment,
profile, socket, arbitrary CLI command, or provider. There is no Cargo target,
build, cache restore, network or account HOME inside this mode. Input may reach
only the private native application. This fake provider does not test a real
Claude service or restore original local conversations.

## Person-owned entry

**Source and mock tests do not admit a native run.** A genuine person must first
review this exact carrier head and package, establish resource/display/tool
admission, and use a terminal outside hosted cagents ancestry. The original
guard remains the first action. An Agent must not run this entry, remove
inherited identity, substitute a User caller, or use another shell/service to
escape a refusal. The prior Agent lifecycle `permission_denied` is preserved.

Once those prerequisites are actually established, the new operator mode is:

```sh
scripts/native-fixture-broker --prebuilt-a48
```

This is a new source implementation, not evidence that the command has been
run successfully. It requires KWin's supported `--virtual` backend, niri's
startup-command interface, dbus-daemon, bubblewrap with `--as-pid-1`, Python,
prlimit and the existing niri-harness. Missing tools or private roots refuse;
there is no fallback to the person's display, HOME, daemon or source builder.

The carrier exclusively creates one fixture's private roots. It starts and
retains the original private session bus, virtual KWin and niri Children. No
host display/focus or host bus is forwarded. The fixed package and offline
provider are read-only held-descriptor mounts; HOME, all five XDG directories,
Wayland socket and project are private. The original UI and daemon Popen owners
remain inside one private PID namespace. A kernel-peer check binds the private
daemon endpoint to its original Child. The private niri socket is held by
inode; Quit is sent on the same peer-verified connection and requires the exact
supported acknowledgement, then the original Child wait.

## Bounded client steps

After the person starts the carrier, the existing client can request:

```sh
scripts/native-fixture start-prebuilt a48-proof
scripts/native-fixture prebuilt-step a48-proof --owner OWNER daemon-start
scripts/native-fixture prebuilt-step a48-proof --owner OWNER ready
scripts/native-fixture prebuilt-step a48-proof --owner OWNER ui-start
```

`OWNER` is the fixture token returned by that actual start, not a caller role or
lifecycle capability. The other named steps are `daemon-status`, `preflight`,
`ui-wait`, `checkpoint`, `daemon-wait` and `empty-stop`. `ready` invokes the
existing matched CLI's `--device local --no-start --json daemon start`, without
autostart. Its success belongs only to that original daemon generation. The
carrier does not give raw preflight capability JSON to the client; complete
stdout/stderr stays in private captures.

Use existing `observe`, `click`, `type`, `key`, `scroll`, `screenshot` and
`status` with the actual owner token. The fixed viewport is 1600 wide; the
operator must verify actual width/height/scale/theme and compare matching
states. A successful launch or screenshot is not recovery proof.

Before **every** checkpoint, empty stop or daemon replacement, normally close
the native **window**, not the pane/provider. Settle profile/draft saves and
publisher writers and then use `ui-wait` for the original UI Child. A live UI
can autostart a competing daemon. A refused or uncertain window/save/wait is a
negative; do not advance. Then, as applicable:

```sh
scripts/native-fixture prebuilt-step a48-proof --owner OWNER checkpoint
scripts/native-fixture prebuilt-step a48-proof --owner OWNER daemon-wait
scripts/native-fixture prebuilt-step a48-proof --owner OWNER daemon-start
scripts/native-fixture prebuilt-step a48-proof --owner OWNER ready
scripts/native-fixture prebuilt-step a48-proof --owner OWNER ui-start
```

`checkpoint` is exactly `prepare-upgrade --yes --no-relaunch-agents` in this
private fixture. It is not a restart verb or promise of provider preservation.
Record the actual helper/provider identity after window close and before
checkpoint, then again after replacement. A fallback/new provider is not
same-helper continuity. All generations and original wait outcomes remain
recorded. A nonzero original UI/daemon exit, pending control Child, failed
readiness, changed package, or unknown wait stops qualification. There is no
automatic retry or next-instance launch after a negative.

## Cleanup and evidence

End only the known private conversation through its normal native control.
Close the window, settle its writes, and wait its original UI. `empty-stop`
uses only `daemon stop --require-empty`; refusal is a negative. Then wait the
original daemon. There is no arbitrary CLI or forced daemon stop endpoint.

Only after those original waits can `stop --keep` drain the fixture:

```sh
scripts/native-fixture stop a48-proof --owner OWNER --keep
```

This mode overrides the ordinary broker's namespace-kill cleanup. It never
signals an unknown provider or adopted child. The relay records kernel-confirmed
exited direct adopted-child receipts with WNOWAIT before consuming each exact
wait; it does not steal an unsettled Popen generation's wait. A zero/wrong PID,
wait error, live/unknown process or uncertain capture remains HOLD. The private
adopted-wait receipt writer must complete before holder settlement. A lost broker
connection does not authorize killing the namespace or dropping its originals.

The private niri receives bounded Quit on the verified original connection;
EOF, malformed reply or timeout is not ACK. After niri's original wait, only
the retained original virtual-display/bus Children may be terminated and waited.
Unknown wait/close/writer/ACK keeps roots and owners. No existing stale root is
automatically adopted or removed. Wrapper22066, old holders and production are
outside this mode. This mode requires `--keep` and intentionally retains all three original
roots, including sandbox captures/screenshots and private display logs/wait
receipts. It does not automatically delete or reuse a completed fixture name;
archiving or removal is a later person-owned disposition after verifying the
original settlement. The current implementation is not a global or
every-exec process census.

Return a private result index with the actual carrier/package source pins,
original requests/exits/waits and sanitized assertion outcomes. Keep raw
capabilities, private transcript/descriptor values, authority, credentials and
account data private. Share only synthetic sentinels, counts, identity equality
booleans/differing field names and checked captures. An independent reviewer
must inspect the actual native evidence and tested head.

Required observations remain separate: empty lifecycle; exact named history
with explicit new-process resume; and conditional same-helper carry. Verify
transcript/draft identity, no prompt replay, no duplicate provider/agent/mail,
and honest uncertainty. Fresh fixture success cannot prove restoration of
original local Claude stores, pending external tools or approvals. Original
conversation/account/store/boot/holder binding is still required. The CB
issued-alias mismatch is unknown and its rejected deny never denied.

Current source/mock tests do not qualify KWin/niri/bwrap integration, a genuine
User entry, package execution, native UI, restart/recovery, real reboot,
responsiveness or production activation. Those remain unrun until the genuine
operator supplies admitted original evidence.
