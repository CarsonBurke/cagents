# cagents for Android

Follow and steer your computer's agents from your phone. The app connects to
the computer over SSH and runs `cagents daemon bridge --mobile` there, which
speaks the [mobile API](../docs/mobile-api.md). Nothing passes through a
server.

What it does:

- **Sessions.** What needs you comes first: notifications and conversations
  waiting for an answer. Then what is working, then the rest, newest first.
  Agents a session started sit beneath it.
- **Conversations.** Follow a chat as it streams. Commands, edits and tool
  calls stay one line each until you tap them.
- **Decisions.** Answer approvals and questions with the options the agent
  offers. Nothing is approved by default.
- **Messages.** Send a message. While a turn runs, steer it now or queue the
  message for after it, or stop the turn.
- **Reconnection.** A dropped connection comes back with backoff, and the app
  watches again what it was showing. In the background the connection closes
  after 30 seconds.

Terminal sessions are listed but can't be opened; the API follows native chats.

## Connect a computer

The computer needs cagents with phone pairing (the phone icon in its status
bar) and an SSH server the phone can reach, running OpenSSH 8.7 or later.

1. Install the app and open it. It makes its own Ed25519 key. Where the phone
   supports it (Android 13 and later), the private key stays in the Android
   Keystore's secure hardware. Elsewhere it is encrypted with a Keystore key.
2. On the computer, click the phone icon in the status bar, then **Pair a
   phone**. It shows a pairing code.
3. In the app, tap **Scan pairing code** and point the camera at the code.
   The app asks for the camera then, and uses it only on that screen.
4. The computer asks whether to allow the phone, showing the phone's key
   fingerprint, which the app shows too. Check that they match and click
   **Allow**.

The app then connects to the computer and shows its sessions. The code gave
it the computer's addresses, SSH port and user, and its host key, which the
app trusts at each address instead of asking on first connection. The
computer authorized the phone's key for the bridge alone, with `command=` and
`restrict`. A code lasts five minutes and works once; a photo of it pairs
nothing without **Allow** on the computer.

The app tries the addresses in the code's order, Tailscale first, giving
each a moment before trying the next as well, so it reaches the computer
from home or away.

If the system camera app reads the code, it can open it in cagents, which
asks before pairing. **Paste a pairing code** takes one copied as text.

### Set up by hand

Without pairing, or for a computer whose cagents can't pair yet, tap **Set up
by hand instead**:

1. Enter the computer's address, SSH port and user. Several addresses,
   separated by commas, are tried in order.
2. Copy the `authorized_keys` line the app shows, or share it to the computer,
   and append it to `~/.ssh/authorized_keys` there:

   ```text
   command="/home/you/.local/bin/cagents daemon bridge --mobile",restrict ssh-ed25519 AAAA... cagents-phone
   ```

   The command must be the absolute path of the installed `cagents`. With
   `command=` and `restrict`, the key can run the bridge and nothing else.
3. Tap **Connect**. On first connection the app shows the computer's host key
   fingerprint. Compare it with
   `ssh-keygen -lf /etc/ssh/ssh_host_ed25519_key.pub` on the computer before
   trusting it. A later change of key is refused until you forget the old one
   in Settings.

The phone and the computer must reach each other: on the same network, or
over a VPN such as Tailscale when away.

## Build

The app is a Kotlin Multiplatform project with a Compose Material 3 Android
app:

- `shared/` is plain Kotlin: the API's messages, the protocol client, the
  conversation model and the reconnecting controller. Its tests run on the JVM.
  They decode `crates/mobile-api/fixtures/v1.jsonl`, the bridge's recording of
  every message, and drive the client against a fake bridge.
- `androidApp/` is the Android app: SSH (sshj), the phone's key, and the
  screens.

Like Rust, it builds on the build host (`CAGENTS_BUILD_REMOTE`, default
`fw16`): the tasks below mirror `mobile/` there with `scripts/gradle-remote`,
run one Gradle build that leaves no daemon behind, and bring back the test
reports and APKs. A Gradle build takes several gigabytes and every core for
minutes, so nothing runs it on the machine you work on: `./gradlew` refuses
to start unless `CAGENTS_GRADLE_LOCAL=1`, which only the build host's runs
and CI set. Set it yourself only to build on this machine deliberately; an
unreachable build host is an error, not a reason to build here. The build host
needs JDK 21 (Gradle provisions it when missing) and the Android SDK, at
`ANDROID_HOME` or `~/Android/Sdk`; installing needs only `adb` here. From the
repository root:

```sh
mise run mobile:test      # protocol and unit tests
mise run mobile:build     # debug APK in mobile/androidApp/build/outputs/apk/debug
mise run mobile:install   # build the preview and install it on the connected phone
mise run mobile:install-debug  # the debuggable build instead
```

Both builds install beside a release build as `dev.cagents.mobile.debug`, and
either replaces the other, keeping the phone's key and trusted hosts.

- The preview is release code: minified by R8, not debuggable, and signed with
  the debug key until release signing exists. Use it on a phone. On a Pixel 8,
  scrolling a list of real sessions had 0.1% janky frames with it, against
  11–25% with the debug build, whose 90th-percentile frame was 32–48 ms.
- The debug build is for `run-as` and the debugger. It leaves its public key
  where adb can read it:

```sh
adb shell run-as dev.cagents.mobile.debug cat files/cagents-phone.pub
```
