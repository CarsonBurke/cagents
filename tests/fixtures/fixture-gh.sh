#!/bin/sh
# Offline GitHub CLI for native-evidence fixtures, installed as the fixture's
# `gh`. Without the switch it refuses everything, so no forge state appears.
# With $FIXTURE/home/.config/fixture-forge present it answers as a gh signed in
# to github.com whose repositories have no pull requests, using the output
# shapes the app's parsers in src/github.rs expect from the real CLI. Nothing
# here reaches the network.
#
# Optional, with the switch on:
#   home/.config/fixture-forge-prs.json  An array of pull requests in the shape
#       `gh pr list --json` prints, each with at least number, url, title,
#       state, headRefName, baseRefName, updatedAt, headRepositoryOwner and
#       headRepository. Each repository lists
#       those whose url is under https://github.com/OWNER/NAME/pull/, in file
#       order.
#   avatars/OWNER.image  Copied into the app's avatar cache, with a fresh
#       modification time, whenever the app resolves a repository. Any PNG,
#       JPEG, GIF or WebP up to 1 MiB and 1024x1024 pixels.
set -eu

# Installed as $FIXTURE/bin/gh. Its own location is the root: gh runs from
# the app and from the daemon, and neither promises to pass the fixture's
# CAGENTS_EVIDENCE_FIXTURE on to what it starts.
fixture=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
config=$fixture/home/.config
if [ ! -e "$config/fixture-forge" ]; then
    printf '%s\n' 'gh: the fixture has no forge access' >&2
    exit 1
fi

login=fixture
viewer_id=U_kgDOFixture
pull_requests=$config/fixture-forge-prs.json

# Opt-in native polling evidence. Record operation labels, never query bodies.
trace() {
    [ -e "$config/fixture-forge-trace" ] || return 0
    printf '%s\t%s\t%s\n' "$(date +%s)" "$PPID" "$1" >> "$config/fixture-forge-requests.tsv"
}

# gh's refusal of a value flag given last, with no value.
need() {
    [ "$1" -ge 2 ] || {
        printf 'flag needs an argument: %s\n' "$2" >&2
        exit 1
    }
}

unsupported() {
    printf 'gh: the fixture forge does not support `gh %s`; it only answers what the app reads\n' "$*" >&2
    exit 1
}

# gh --include prints the status line, then headers ending in CRLF.
headers() {
    status=$1 resource=$2
    printf 'HTTP/2.0 %s\n' "$status"
    printf '%s\r\n' \
        'Content-Type: application/json; charset=utf-8' \
        'X-Github-Media-Type: github.v3; format=json' \
        'X-Ratelimit-Limit: 5000' \
        'X-Ratelimit-Remaining: 4999' \
        "X-Ratelimit-Reset: $(($(date +%s) + 3600))" \
        "X-Ratelimit-Resource: $resource" \
        'X-Ratelimit-Used: 1' \
        ''
}

# OWNER/NAME of a github.com remote URL, or nothing for any other host.
github_slug() {
    url=${1%/}
    url=${url%.git}
    printf '%s\n' "$url" | sed -nE \
        's#^(https?://([^@/]*@)?|ssh://([^@/]*@)?|git://|[^@/:]+@)?(www\.)?[Gg][Ii][Tt][Hh][Uu][Bb]\.[Cc][Oo][Mm](:[0-9]+)?[:/]([A-Za-z0-9-]+)/([A-Za-z0-9._-]+)$#\6/\7#p'
}

repository_json() {
    printf '{"nameWithOwner":"%s","url":"https://github.com/%s"}\n' "$1" "$1"
}

# The app's cache directory is its storage name, lower-cased by the
# directories crate; it accepts a cached image younger than seven days.
seed_avatars() {
    [ -d "$fixture/avatars" ] || return 0
    storage=$(printf 'cagents-development%s' "${CAGENTS_DEVELOPMENT_INSTANCE:+-$CAGENTS_DEVELOPMENT_INSTANCE}" |
        tr '[:upper:]' '[:lower:]')
    cache=${XDG_CACHE_HOME:-${HOME:-}/.cache}/$storage/github-avatars
    for image in "$fixture"/avatars/*.image; do
        [ -f "$image" ] || continue
        owner=$(basename -- "$image" .image | tr '[:upper:]' '[:lower:]')
        cached=$cache/github.com-$owner.image
        # A seed that can't be written leaves the app without that avatar,
        # as a failed download would; it never fails the repository's read.
        mkdir -p -- "$cache" 2>/dev/null || return 0
        if cmp -s -- "$image" "$cached"; then
            touch -- "$cached" 2>/dev/null || :
        else
            # Published whole: the app may be reading the previous copy.
            cp -- "$image" "$cached.$$" 2>/dev/null &&
                mv -f -- "$cached.$$" "$cached" 2>/dev/null ||
                rm -f -- "$cached.$$"
        fi
    done
}

# gh's own order for choosing a repository among a checkout's remotes.
repo_view() {
    slug=
    while [ "$#" -gt 0 ]; do
        case $1 in
            --json | -q | --jq | -t | --template | -b | --branch) need "$#" "$1"; shift ;;
            --json=* | --jq=* | --template=* | --branch=* | -w | --web) ;;
            -*) ;;
            *) slug=$1 ;;
        esac
        shift
    done
    seed_avatars
    if [ -n "$slug" ]; then
        repository_json "${slug#github.com/}"
        return
    fi
    if ! git rev-parse --git-dir >/dev/null 2>&1; then
        printf '%s\n' 'failed to run git: fatal: not a git repository (or any of the parent directories): .git' >&2
        exit 1
    fi
    remotes=$(git remote)
    if [ -z "$remotes" ]; then
        printf '%s\n' 'no git remotes found' >&2
        exit 1
    fi
    # As gh does, the remote `gh repo set-default` marked comes first, then
    # upstream, github, origin and the rest.
    resolved=$(git config --get-regexp '^remote\..*\.gh-resolved$' 2>/dev/null |
        sed -n 's/^remote\.\(.*\)\.gh-resolved base$/\1/p' || :)
    for name in $resolved upstream github origin $(printf '%s\n' "$remotes" | grep -vx -e upstream -e github -e origin || :); do
        printf '%s\n' "$remotes" | grep -qxF -- "$name" || continue
        slug=$(github_slug "$(git remote get-url -- "$name")")
        if [ -n "$slug" ]; then
            repository_json "$slug"
            return
        fi
    done
    printf '%s\n' 'none of the git remotes configured for this repository point to a known GitHub host. To tell gh about a new GitHub host, please use `gh auth login`' >&2
    exit 1
}

auth_status() {
    host= json=
    while [ "$#" -gt 0 ]; do
        case $1 in
            --hostname | -h) need "$#" "$1"; shift; host=${1:-} ;;
            --hostname=*) host=${1#*=} ;;
            --json) need "$#" "$1"; shift; json=${1:-} ;;
            --json=*) json=${1#*=} ;;
        esac
        shift
    done
    if [ -n "$json" ] && [ "$json" != hosts ]; then
        printf 'Unknown JSON field: "%s"\nAvailable fields:\n  hosts\n' "$json" >&2
        exit 1
    fi
    if [ -n "$host" ] && [ "$(printf '%s' "$host" | tr '[:upper:]' '[:lower:]')" != github.com ]; then
        printf 'You are not logged into any accounts on %s\n' "$host" >&2
        if [ -n "$json" ]; then
            printf '%s\n' '{"hosts":{}}'
            exit 0
        fi
        exit 1
    fi
    if [ -n "$json" ]; then
        printf '{"hosts":{"github.com":[{"state":"success","active":true,"host":"github.com","login":"%s","tokenSource":"keyring","scopes":"gist, read:org, repo, workflow","gitProtocol":"https"}]}}\n' "$login"
    else
        printf '%s\n' 'github.com' \
            "  ✓ Logged in to github.com account $login (keyring)" \
            '  - Active account: true' \
            '  - Git operations protocol: https' \
            '  - Token: gho_************************************' \
            "  - Token scopes: 'gist', 'read:org', 'repo', 'workflow'"
    fi
}

# The seeded pull requests of OWNER/NAME.
seeded() {
    if [ ! -f "$pull_requests" ]; then
        printf '[]\n'
        return
    fi
    jq -c --arg prefix "https://github.com/$1/pull/" '
        map(select(.url | ascii_downcase | startswith($prefix | ascii_downcase)))' "$pull_requests"
}

# Keeps only the requested --json fields, as gh prints them.
select_fields() {
    jq -c --arg fields "$1" '($fields | split(",")) as $keep
        | def pick: with_entries(select(.key as $key | $keep | index($key)));
        if type == "array" then map(pick) else pick end'
}

# A seed the app could not parse fails here, naming the file: in the app it
# would be a failed read, which pauses that host's refreshes. These are the
# fields src/github.rs's PullRequest requires.
check_seed() {
    [ -f "$pull_requests" ] || return 0
    if ! jq -e 'type == "array" and all(.[]; type == "object"
            and (.number | type) == "number"
            and all(.url, .title, .headRefName, .baseRefName, .updatedAt; type == "string")
            and (.state | IN("OPEN", "CLOSED", "MERGED"))
            and (.headRepositoryOwner.login | type) == "string"
            and (.headRepository.name | type) == "string")' "$pull_requests" >/dev/null 2>&1; then
        printf 'gh: %s must be a JSON array of pull requests, each with a number, url, title, state (OPEN, CLOSED or MERGED), headRefName, baseRefName, updatedAt, headRepositoryOwner.login and headRepository.name\n' "$pull_requests" >&2
        exit 1
    fi
}

pr_not_found() {
    printf 'GraphQL: Could not resolve to a PullRequest with the number of %s. (repository.pullRequest)\n' "$1" >&2
    exit 1
}

pr() {
    subcommand=${1:-}
    [ "$#" -eq 0 ] || shift
    slug= state=open head= author= limit=30 fields= number=
    while [ "$#" -gt 0 ]; do
        case $1 in
            -R | --repo) need "$#" "$1"; shift; slug=${1#github.com/} ;;
            --repo=*) slug=${1#*=}; slug=${slug#github.com/} ;;
            -s | --state) need "$#" "$1"; shift; state=$1 ;;
            --state=*) state=${1#*=} ;;
            -H | --head) need "$#" "$1"; shift; head=$1 ;;
            --head=*) head=${1#*=} ;;
            -A | --author) need "$#" "$1"; shift; author=$1 ;;
            --author=*) author=${1#*=} ;;
            -L | --limit) need "$#" "$1"; shift; limit=$1 ;;
            --limit=*) limit=${1#*=} ;;
            --json) need "$#" "$1"; shift; fields=$1 ;;
            --json=*) fields=${1#*=} ;;
            -S | --search | -B | --base | -l | --label | -a | --assignee | --app | -q | --jq | -t | --template) need "$#" "$1"; shift ;;
            -*) ;;
            *) number=$1 ;;
        esac
        shift
    done
    if [ -z "$slug" ]; then
        slug=$(repo_view | sed -nE 's#^\{"nameWithOwner":"([^"]+)".*#\1#p')
    fi
    case $subcommand in
        list | view | checks)
            # Without --json gh prints a table for people, which the app never reads.
            [ -n "$fields" ] || unsupported pr "$subcommand" without --json
            ;;
        *) unsupported pr "$subcommand" ;;
    esac
    check_seed
    case $subcommand in
        list)
            trace "pr list author=$author repository=$slug"
            case $state in
                open | closed | merged | all) ;;
                *)
                    printf 'invalid argument "%s" for "-s, --state" flag: valid values are {open|closed|merged|all}\n' "$state" >&2
                    exit 1
                    ;;
            esac
            [ "$author" != @me ] || author=$login
            if [ ! -f "$pull_requests" ]; then
                printf '[]\n'
                return
            fi
            seeded "$slug" | jq -c --arg state "$state" --arg head "$head" --arg author "$author" \
                --argjson limit "$limit" '
                map(select(
                    ($state == "all"
                        or ($state == "open" and .state == "OPEN")
                        or ($state == "closed" and (.state == "CLOSED" or .state == "MERGED"))
                        or ($state == "merged" and .state == "MERGED"))
                    and ($head == "" or .headRefName == $head)
                    and ($author == "" or .author.login == $author)))
                | .[:$limit]' | select_fields "$fields"
            ;;
        view | checks)
            trace "pr $subcommand repository=$slug"
            case $number in
                '' | *[!0-9]*) unsupported pr "$subcommand" "$number" ;;
            esac
            found=$(seeded "$slug" | jq -c --argjson number "$number" \
                'map(select(.number == $number)) | .[0] // empty')
            [ -n "$found" ] || pr_not_found "$number"
            if [ "$subcommand" = view ]; then
                printf '%s\n' "$found" | select_fields "$fields"
            else
                printf "no checks reported on the '%s' branch\n" \
                    "$(printf '%s\n' "$found" | jq -r .headRefName)" >&2
                exit 1
            fi
            ;;
    esac
}

api() {
    include=false method= endpoint= query= number= fields=false owner= name= limit=50
    search_open= search_all= open_limit=50 all_limit=100 if_none_match=
    while [ "$#" -gt 0 ]; do
        case $1 in
            -i | --include) include=true ;;
            -X | --method) need "$#" "$1"; shift; method=$1 ;;
            -H | --header)
                need "$#" "$1"
                shift
                case $1 in
                    [Ii][Ff]-[Nn]one-[Mm]atch:*) if_none_match=$(printf '%s' "${1#*:}" | sed 's/^ *//') ;;
                esac
                ;;
            --hostname | --input | -q | --jq | -t | --template | --cache) need "$#" "$1"; shift ;;
            -f | --raw-field | -F | --field)
                need "$#" "$1"
                shift
                case $1 in
                    query=*) query=${1#query=} ;;
                    number=*) number=${1#number=} ;;
                    owner=*) owner=${1#owner=} ;;
                    name=*) name=${1#name=} ;;
                    limit=*) limit=${1#limit=} ;;
                    open=*) search_open=${1#open=} ;;
                    all=*) search_all=${1#all=} ;;
                    openLimit=*) open_limit=${1#openLimit=} ;;
                    allLimit=*) all_limit=${1#allLimit=} ;;
                esac
                fields=true
                ;;
            -*) ;;
            *) endpoint=$1 ;;
        esac
        shift
    done
    # As in gh, parameters make a request a POST unless a method is given.
    if [ -z "$method" ]; then
        if "$fields"; then method=POST; else method=GET; fi
    fi
    if [ "$endpoint" = graphql ] && [ -n "$search_all" ]; then
        # The Work board's authored listing: both searches name the
        # repository as repo:OWNER/NAME and the viewer as @me.
        slug=$(printf '%s\n' "$search_all" | sed -nE 's#.*repo:([^ ]+).*#\1#p')
        trace "api graphql CagentsAuthoredPullRequests repository=$slug"
        check_seed
        ! "$include" || headers '200 OK' graphql
        seeded "$slug" | jq -c --arg login "$login" --argjson open "$open_limit" --argjson all "$all_limit" '
            map(select(.author.login == $login)) | sort_by(.updatedAt) | reverse
            | def identity: {number, title, state, isDraft, headRefName, headRefOid,
                headRepository, headRepositoryOwner, baseRefName, updatedAt, url};
            {data: {viewer: {login: $login},
                open: {nodes: (map(select(.state == "OPEN")) | .[:$open] | map(identity + {
                    reviewDecision, additions, deletions,
                    commits: {nodes: [{commit: {statusCheckRollup:
                        (if (.statusCheckRollup // [] | length) == 0 then null
                         else {contexts: {nodes: (.statusCheckRollup | map(
                            if .__typename == "CheckRun"
                            then del(.workflowName) + {checkSuite: (if .workflowName
                                then {workflowRun: {workflow: {name: .workflowName}}} else null end)}
                            else . end))}} end)}}]}}))},
                all: {nodes: (.[:$all] | map(identity))}}}'
        return
    fi
    if [ "$endpoint" = graphql ]; then
        operation=$(printf '%s\n' "$query" | sed -nE '1s/^[[:space:]]*(query|mutation)[[:space:]]+([A-Za-z0-9_]+).*/\2/p')
        trace "api graphql ${operation:-anonymous} repository=$owner/$name"
        case $query in
            'query SidebarPullRequests('* | 'query SidebarHistory('*)
                check_seed
                ! "$include" || headers '200 OK' graphql
                open=false
                case $query in 'query SidebarPullRequests('*) open=true ;; esac
                seeded "$owner/$name" | jq -c --argjson open "$open" --argjson limit "$limit" '
                    def aggregate:
                        map((.conclusion // .state // .status // "UNKNOWN") | ascii_upcase) as $states
                        | if any($states[]; IN("FAILURE", "ERROR", "CANCELLED", "TIMED_OUT", "ACTION_REQUIRED", "STARTUP_FAILURE", "STALE")) then "FAILURE"
                          elif any($states[]; IN("PENDING", "QUEUED", "IN_PROGRESS", "REQUESTED", "WAITING", "EXPECTED")) then "PENDING"
                          elif all($states[]; IN("SUCCESS", "NEUTRAL", "SKIPPED")) then "SUCCESS"
                          else "ERROR" end;
                    map(select(($open | not) or .state == "OPEN")) | .[:$limit]
                    | map(.headRepository.owner = .headRepositoryOwner
                        | if $open then
                            .commits = {nodes:[{commit:{statusCheckRollup:
                                (if (.statusCheckRollup // [] | length) == 0 then null
                                 else {state:(.statusCheckRollup | aggregate)} end)}}]}
                          else del(.statusCheckRollup, .commits) end)
                    | {data:{rateLimit:{cost:1},repository:{pullRequests:{nodes:.}}}}'
                ;;
            *'pullRequests(first:'*)
                # The pull request browser, and the review-request poll, which
                # also asks who is reading: no open pull requests to list.
                viewer=
                case $query in
                    *'viewer{id login}'*) viewer='"viewer":{"id":"'$viewer_id'","login":"'$login'"},' ;;
                esac
                ! "$include" || headers '200 OK' graphql
                printf '{"data":{%s"repository":{"pullRequests":{"nodes":[],"pageInfo":{"hasNextPage":false,"endCursor":null}}}}}\n' "$viewer"
                ;;
            *'search(type:ISSUE'*)
                ! "$include" || headers '200 OK' graphql
                printf '%s\n' '{"data":{"search":{"nodes":[],"pageInfo":{"hasNextPage":false,"endCursor":null}}}}'
                ;;
            *'pullRequest(number:'*)
                # GitHub answers 200 with an error; gh prints it and fails.
                message="Could not resolve to a PullRequest with the number of ${number:-0}."
                ! "$include" || headers '200 OK' graphql
                printf '{"data":{"repository":{"pullRequest":null}},"errors":[{"type":"NOT_FOUND","path":["repository","pullRequest"],"locations":[{"line":1,"column":1}],"message":"%s"}]}\n' "$message"
                printf 'gh: %s\n' "$message" >&2
                exit 1
                ;;
            *) unsupported api graphql ;;
        esac
        return
    fi
    trace "api $method ${endpoint%%\?*}"
    case $method:$endpoint in
        GET:repos/*/issues\?*)
            # The most recently updated issues and pull requests, the
            # viewer's with creator=, whose validator changes with the seeded
            # pull requests it covers.
            slug=${endpoint#repos/}
            slug=${slug%%/issues*}
            creator=$(printf '%s\n' "$endpoint" | sed -nE 's#.*[?&]creator=([^&]+).*#\1#p')
            per_page=$(printf '%s\n' "$endpoint" | sed -nE 's#.*[?&]per_page=([0-9]+).*#\1#p')
            check_seed
            page=$(seeded "$slug" | jq -c --arg creator "$creator" --argjson limit "${per_page:-30}" '
                map(select($creator == "" or .author.login == $creator))
                | sort_by(.updatedAt) | reverse | .[:$limit]
                | map({number, title, state: (if .state == "OPEN" then "open" else "closed" end),
                    updated_at: .updatedAt, html_url: .url, pull_request: {html_url: .url}})')
            etag="W/\"$(printf '%s' "$page" | cksum | cut -d' ' -f1)\""
            if [ "$if_none_match" = "$etag" ]; then
                ! "$include" || headers '304 Not Modified' core
                printf '%s\n' 'gh: HTTP 304' >&2
                exit 1
            fi
            if "$include"; then
                headers '200 OK' core | sed '$d'
                printf 'Etag: %s\r\n\r\n' "$etag"
            fi
            printf '%s\n' "$page"
            ;;
        GET:repos/*/releases | GET:repos/*/releases\?*)
            ! "$include" || headers '200 OK' core
            printf '[]\n'
            ;;
        *)
            ! "$include" || headers '404 Not Found' core
            printf '%s' '{"message":"Not Found","documentation_url":"https://docs.github.com/rest","status":"404"}'
            printf '%s\n' 'gh: Not Found (HTTP 404)' >&2
            exit 1
            ;;
    esac
}

case ${1:-} in
    --version | version)
        printf '%s\n' 'gh version 2.102.0 (2026-09-30)' \
            'https://github.com/cli/cli/releases/tag/v2.102.0'
        ;;
    auth)
        shift
        [ "${1:-}" = status ] || unsupported auth "$@"
        shift
        trace "auth status"
        auth_status "$@"
        ;;
    repo)
        shift
        [ "${1:-}" = view ] || unsupported repo "$@"
        shift
        trace "repo view"
        repo_view "$@"
        ;;
    pr) shift; pr "$@" ;;
    api) shift; api "$@" ;;
    *) unsupported "$@" ;;
esac
