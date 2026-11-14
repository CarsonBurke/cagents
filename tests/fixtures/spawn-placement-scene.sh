# Offline native scene for issue #2735, used identically on both revisions.
set -euo pipefail
fx=$(evidence env sh -c 'printf %s "$CAGENTS_EVIDENCE_FIXTURE"' | tail -1)
config=$fx/home/.claude
mkdir -p "$fx/casmi" "$config"
printf '1\n' >"$config/fixture-first-turn-seconds"
printf 'Fixture reply.\n' >"$config/fixture-reply.md"
rm -f "$config/fixture-run-once"
parent_tab() {
    evidence drive move 500,22 >/dev/null
    evidence drive scroll horizontal -100 >/dev/null
    sleep 1
    evidence drive click 310,22 >/dev/null
    sleep 2
}
shot() {
    evidence drive move 1,1 >/dev/null
    sleep 1
    evidence shot "$1"
}
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
evidence drive key ctrl+shift+i
sleep 2
evidence drive click 'Backoff implementation'
sleep 2
parent_tab
sleep 4
evidence drive observe
shot spawn-start
printf '%s\n' "cagents agent spawn --name reviewer --model haiku --surface chat 'Review the one-row cards'" >"$config/fixture-run-once"
evidence cli agent send "$parent" 'Start a reviewer on the one-row cards.'
sleep 10
evidence cli --json channel list
evidence cli --json agent list
evidence drive observe
shot same-project
if [ "$SIDE" = after ]; then
    evidence drive click 'Open'
    sleep 3
    evidence drive observe
    shot same-project-opened
fi
parent_tab
sleep 2
printf '%s\n' "cagents agent spawn --name casmi-reviewer --model haiku --surface chat --project '$fx/casmi' 'Review the casmi build'" >"$config/fixture-run-once"
evidence cli agent send "$parent" 'Start a reviewer on the casmi build.'
sleep 10
evidence cli --json channel list
evidence cli --json agent list
evidence drive observe
shot cross-project
