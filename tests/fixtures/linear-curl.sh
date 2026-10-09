#!/bin/sh
# Fixture transport: inspect requests without sending credentials over the network.
set -eu
printf '%s\n' "$@" > "$LINEAR_FIXTURE/argv"
cat > "$LINEAR_FIXTURE/request"
printf 'request\n' >> "$LINEAR_FIXTURE/calls"
if [ -f "$LINEAR_FIXTURE/fail" ]; then exit 28; fi
case "$(cat "$LINEAR_FIXTURE/request")" in
  *issueUpdate*|*issueCreate*|*commentCreate*|*attachmentCreate*|*issueRelationCreate*|*issueRelationDelete*)
    cat "$LINEAR_FIXTURE/mutation" ;;
  *'team(id:$id)'*)
    if [ -f "$LINEAR_FIXTURE/parent-team" ] && ! /usr/bin/grep -q '2b498c52-7dc7-4c76-ad59-dfc488c40222' "$LINEAR_FIXTURE/request"; then
      cat "$LINEAR_FIXTURE/parent-team"
    else
      cat "$LINEAR_FIXTURE/read"
    fi ;;
  *) cat "$LINEAR_FIXTURE/read" ;;
esac
printf '\n200'
