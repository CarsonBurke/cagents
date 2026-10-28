# Offline chat composer validation

`tests/fixtures/chat-composer-claude.sh` is a fake Claude stream-json executable
for native UI checks. It requires `jq`, makes no network requests, and uses a
synthetic account. It advertises asynchronous message support, holds its first
turn busy, acknowledges queued-message removal, and settles on interrupt.
Later turns complete immediately. Busy messages deliberately stay queued;
this fixture checks composer interactions, not provider delivery.

Use a new directory beneath the repository's ignored `.worktrees/` for every
run. Keep its HOME, XDG data/config/cache, daemon runtime, and project isolated
from the development application you normally use. For example, from the
checkout being validated:

```sh
fixture_root="$PWD/.worktrees/chat-composer-fixture"
fixture_runtime=$(mktemp -d "$XDG_RUNTIME_DIR/composer.XXXXXX")
mkdir -p "$fixture_root"/{home,data,config,cache,bin,project}
ln -s "$PWD/tests/fixtures/chat-composer-claude.sh" "$fixture_root/bin/claude"
git init --initial-branch=main "$fixture_root/project"
```

Run the UI through `scripts/run-development --browserless`. Prefix both the
runner and every CLI command with the same fixture environment:

```sh
fixture_env() {
  env -u CAGENTS_AGENT_AUTHORITY \
    HOME="$fixture_root/home" XDG_RUNTIME_DIR="$fixture_runtime" \
    XDG_DATA_HOME="$fixture_root/data" XDG_CONFIG_HOME="$fixture_root/config" \
    XDG_CACHE_HOME="$fixture_root/cache" PATH="$fixture_root/bin:$PATH" \
    CARGO_HOME="${CARGO_HOME:-$HOME/.cargo}" \
    RUSTUP_HOME="${RUSTUP_HOME:-$HOME/.rustup}" "$@"
}
fixture_env scripts/run-development --browserless --debug --log-file "$fixture_root/ui.log"
```

Keep the existing Cargo target/cache configuration when using a worktree; do
not start another build while the runner builds its matched executables. For
a nested Niri session, use `niri-harness session start ... --no-window-wait`
with a wrapper containing that runner command. Before replacing
`XDG_RUNTIME_DIR`, convert the inherited relative `WAYLAND_DISPLAY` to an
absolute path (`$XDG_RUNTIME_DIR/$WAYLAND_DISPLAY`). Keep the inherited
`NIRI_SOCKET`, and use only this owned nested session for input and screenshots.
The runtime directory needs a short path because Unix sockets have a path-length
limit; keep it under the original XDG runtime directory rather than the checkout.

After the UI starts, use its newly built `cagents` CLI under the fixture
environment to create the project and synthetic agent:

```sh
fixture_cli="${CARGO_TARGET_DIR:-$PWD/target}/dev-runtime/cagents"
fixture_env "$fixture_cli" project open "$fixture_root/project"
fixture_env "$fixture_cli" --json agent spawn --provider claude --model haiku --surface chat \
  --repository "$fixture_root/project" \
  --prompt 'Can you investigate why the Kaggle submission failed?'
```

Open the resulting chat in the UI and check:

1. Enter a message and choose the **Steer** icon; hover its queued timing icon
   to confirm next-step delivery.
2. Enter a long URL and choose the **Queue** icon; hover its timing icon to
   confirm next-turn delivery. Its preview truncates within the available width
   without hiding its actions.
3. Capture the busy composer, including a separate unsent draft.
4. Recall a queued message with the **Edit** pencil; confirm its text moves into the
   composer and its queued entry disappears. Queue it again if needed.
5. Remove every queued entry, then click the **Stop** icon. Confirm the composer returns
   to its idle send state. Clear the draft and capture that state.
6. Repeat the queued-state checks at a narrow window width, including keyboard
   focus and the split composer toolbar.

Store screenshots under `docs/screenshots/` for PR evidence. Stop the owned
nested session, close its synthetic agent, and stop only its isolated daemon
when finished. The fixture directory can then be removed.

The daemon removes `CAGENTS_*` variables from a provider's environment, so
optional fixture behaviors are switched on by files in
`$fixture_root/home/.claude`, the fixture's Claude config directory. The
error files are read for every prompt; the others take effect from the
provider's next start.

For provider failure checks, write a diagnostic such as `Failed to
authenticate: OAuth session expired and could not be refreshed` to
`fixture-image-prompt-error`. Any prompt containing an image then settles with
that error before any reply. One Send of text plus images must show one failure
notice.

To check where a failure is drawn after delivered work, write a diagnostic to
`fixture-delivery-error` instead. Every prompt then gets a reply, a background
command's completion notice, a follow-up, and that error. The notice must
appear once, after the follow-up.

For tool disclosure checks, create `fixture-verbosity`. Each turn emits a
streamed MCP call with a large prompt, a Skill call, a failed MCP call, and a
multiline Bash command. It records complete tool inputs and results in the
synthetic provider history, so the same transcript can be reopened.

In the native chat, expand the activity group and check that the closed MCP
row says `src/app.rs`, the Skill row says `writing-copy`, and neither shows
internal framing. The failed `private.txt` step must retain `Permission denied`.
The shell preview must show `cat <<EOF` without its heredoc payload. Expand
individual steps and verify their complete Input/Output remains selectable
and copyable. Collapse them while the first turn is still busy, stop that
turn, and reopen the saved chat; compare the completed and restored content.
The fixture represents a reproducible payload path, not a captured user
conversation or evidence about the provider's answer prose.

For image preview and viewer checks, copy a PNG to
`"$fixture_root/home/fixture-image.png"` before spawning the agent. Native
launches drop `CAGENTS_*` variables, so the fixture reads the image from its
provider home; `CAGENTS_FIXTURE_IMAGE` overrides the path when the script runs
directly. Each turn then also returns that image as a provider-produced base64
image block, recorded in the synthetic history like other frames. Paste an image into the composer to
cover a user-attached image in the same transcript. Use synthetic images only.
