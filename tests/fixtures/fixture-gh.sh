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
#       `gh pr list --json` prints. Each repository lists those whose url is
#       under https://github.com/OWNER/NAME/pull/, in file order.
#   avatars/OWNER.image  Copied into the app's avatar cache, with a fresh
#       modification time, whenever the app resolves a repository. Any PNG,
#       JPEG, GIF or WebP up to 1 MiB and 1024x1024 pixels.
set -eu

# Installed as $FIXTURE/bin/gh; the daemon removes CAGENTS_* variables from
# the environment of what it starts, so the location is the reliable root.
fixture=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
config=$fixture/home/.config
if [ ! -e "$config/fixture-forge" ]; then
    printf '%s\n' 'gh: the fixture has no forge access' >&2
    exit 1
fi

login=fixture
viewer_id=U_kgDOFixture
pull_requests=$config/fixture-forge-prs.json

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

seed_avatars() {
    [ -d "$fixture/avatars" ] || return 0
    instance=${CAGENTS_DEVELOPMENT_INSTANCE:+-$CAGENTS_DEVELOPMENT_INSTANCE}
    cache=${XDG_CACHE_HOME:-$HOME/.cache}/cagents-development$instance/github-avatars
    for image in "$fixture"/avatars/*.image; do
        [ -f "$image" ] || continue
        owner=$(basename -- "$image" .image | tr '[:upper:]' '[:lower:]')
        mkdir -p -- "$cache"
        cp -- "$image" "$cache/github.com-$owner.image"
        touch -- "$cache/github.com-$owner.image"
    done
}

# gh's own order for choosing a repository among a checkout's remotes.
repo_view() {
    slug=
    while [ "$#" -gt 0 ]; do
        case $1 in
            --json | -q | --jq | -t | --template | -b | --branch) shift ;;
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
    for name in upstream github origin $(printf '%s\n' "$remotes" | grep -vx -e upstream -e github -e origin || :); do
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
            --hostname | -h) shift; host=${1:-} ;;
            --hostname=*) host=${1#*=} ;;
            --json) shift; json=${1:-} ;;
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

pr_not_found() {
    printf 'GraphQL: Could not resolve to a PullRequest with the number of %s. (repository.pullRequest)\n' "$1" >&2
    exit 1
}

pr() {
    subcommand=${1:-}
    shift || :
    slug= state=open head= author= limit=30 fields= number=
    while [ "$#" -gt 0 ]; do
        case $1 in
            -R | --repo) shift; slug=${1#github.com/} ;;
            -s | --state) shift; state=$1 ;;
            -H | --head) shift; head=$1 ;;
            -A | --author) shift; author=$1 ;;
            -L | --limit) shift; limit=$1 ;;
            --json) shift; fields=$1 ;;
            -S | --search | -B | --base | -l | --label | -a | --assignee | --app | -q | --jq | -t | --template) shift ;;
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
    case $subcommand in
        list)
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
    include=false method= endpoint= query= number= fields=false
    while [ "$#" -gt 0 ]; do
        case $1 in
            -i | --include) include=true ;;
            -X | --method) shift; method=$1 ;;
            --hostname | -H | --header | --input | -q | --jq | -t | --template | --cache) shift ;;
            -f | --raw-field | -F | --field)
                shift
                case $1 in
                    query=*) query=${1#query=} ;;
                    number=*) number=${1#number=} ;;
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
    if [ "$endpoint" = graphql ]; then
        case $query in
            *'viewer{id login}'*'pullRequests(first:'*)
                # The review-request poll: nobody asked for a review.
                body='{"data":{"viewer":{"id":"'$viewer_id'","login":"'$login'"},"repository":{"pullRequests":{"nodes":[],"pageInfo":{"hasNextPage":false,"endCursor":null}}}}}'
                ! "$include" || headers '200 OK' graphql
                printf '%s\n' "$body"
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
    case $method:$endpoint in
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
        auth_status "$@"
        ;;
    repo)
        shift
        [ "${1:-}" = view ] || unsupported repo "$@"
        shift
        repo_view "$@"
        ;;
    pr) shift; pr "$@" ;;
    api) shift; api "$@" ;;
    *) unsupported "$@" ;;
esac
