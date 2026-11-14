# Offline native scene for issue #2735, used identically on both revisions.
set -euo pipefail
fx=$(evidence env sh -c 'printf %s "$CAGENTS_EVIDENCE_FIXTURE"' | tail -1)
config=$fx/home/.claude
mkdir -p "$fx/casmi" "$config"
printf '1\n' >"$config/fixture-first-turn-seconds"
printf 'Fixture reply.\n' >"$config/fixture-reply.md"
rm -f "$config/fixture-run-once"
evidence cli project open "$fx/project"
evidence cli project open "$fx/casmi"
channel=$(evidence cli --json channel spawn --repository "$fx/project" --title 'Backoff implementation' | jq -er '.data.channel.id')
spawn() {
    evidence cli --json agent spawn --provider claude --model haiku --surface chat --repository "$fx/project" --channel "$channel" "$@"
}
parent=$(spawn --name lead --prompt 'Review PR #2689 before it merges.' | jq -er '.data.agent.id')
sleep 3
spawn --name retry --prompt 'Review the retry change for restart safety.' >/dev/null
sleep 3
spawn --name jitter --prompt 'Check the jitter range against the spec.' >/dev/null
sleep 5
evidence drive observe
evidence drive click 'Backoff implementation'
sleep 4
evidence drive observe
evidence shot spawn-start
printf '%s\n' "cagents agent spawn --name reviewer --model haiku --surface chat 'Review the one-row cards'" >"$config/fixture-run-once"
evidence cli agent send "$parent" 'Start a reviewer on the one-row cards.'
sleep 10
evidence cli --json channel list
evidence drive observe
evidence shot same-project
if [ "$SIDE" = after ]; then
    evidence drive click 'Open agent'
    sleep 3
    evidence drive observe
    evidence shot same-project-opened
    evidence drive click 'lead'
    sleep 2
fi
printf '%s\n' "cagents agent spawn --name casmi-reviewer --model haiku --surface chat --project '$fx/casmi' 'Review the casmi build'" >"$config/fixture-run-once"
evidence cli agent send "$parent" 'Start a reviewer on the casmi build.'
sleep 10
evidence cli --json channel list
evidence drive observe
evidence shot cross-project
