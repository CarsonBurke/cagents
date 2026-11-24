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

The computer needs cagents 0.2.3 or later and an SSH server the phone can
reach.

1. Install the app and open it. It makes its own Ed25519 key. Where the phone
   supports it (Android 13 and later), the private key stays in the Android
   Keystore's secure hardware. Elsewhere it is encrypted with a Keystore key.
2. Enter the computer's address, SSH port and user.
3. Copy the `authorized_keys` line the app shows, or share it to the computer,
   and append it to `~/.ssh/authorized_keys` there:

   ```text
   command="/home/you/.local/bin/cagents daemon bridge --mobile",restrict ssh-ed25519 AAAA... cagents-phone
   ```

   The command must be the absolute path of the installed `cagents`. With
   `command=` and `restrict`, the key can run the bridge and nothing else.
4. Tap **Connect**. On first connection the app shows the computer's host key
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
minutes, so don't run `./gradlew` on a laptop someone is using. The build host
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
