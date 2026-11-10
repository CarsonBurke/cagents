#!/usr/bin/python3
"""No-network coverage for the gh shim's transparency and safe telemetry."""
import contextlib
import importlib.machinery
import io
import fcntl
import importlib.util
import json
import os
import pty
from pathlib import Path
import signal
import subprocess
import tempfile
import time
import unittest
from types import SimpleNamespace
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[1]
SCRIPT = ROOT / "scripts/gh-observe"
loader = importlib.machinery.SourceFileLoader("gh_observe", str(SCRIPT))
spec = importlib.util.spec_from_loader(loader.name, loader)
observe = importlib.util.module_from_spec(spec)
loader.exec_module(observe)


class ParserTests(unittest.TestCase):
    def parse(self, request, response, status=200, headers=None):
        rows = []
        parser = observe.Traffic({"command": "api graphql"}, rows.append)
        wire = "* Request at 2026-11-09\n* Request to https://api.github.com/graphql\n> POST /graphql HTTP/1.1\n> Authorization: token secret-token\n\n"
        wire += json.dumps(request, indent=2) + "\n"
        wire += f"< HTTP/2.0 {status} Response\n< Content-Type: application/json\n"
        for key, value in (headers or {}).items():
            wire += f"< {key}: {value}\n"
        wire += "< Set-Cookie: secret-cookie\n\n" + json.dumps(response, indent=2) + "\n* Request took 1ms\n"
        for line in wire.encode().splitlines(keepends=True):
            self.assertTrue(parser.feed(line))
        return rows

    def test_cost_operation_errors_and_allowlisted_headers(self):
        rows = self.parse({"query": 'query FindPRs { repository(owner:"private-owner", name:"secret-repo") { pullRequests(first:20) { totalCount } } rateLimit {cost} }', "variables": {"token": "secret-variable"}}, {"data": {"rateLimit": {"cost": 7}}, "errors": [{"type": "RATE_LIMITED", "message": "API rate limit exceeded for secret-login"}]}, headers={"X-RateLimit-Resource": "graphql", "Retry-After": "120", "X-RateLimit-Used": "775"})
        self.assertEqual(len(rows), 1)
        row = rows[0]
        self.assertEqual(row["operation"], "FindPRs")
        self.assertEqual(row["primary_cost"], 7)
        self.assertEqual(row["duration_ms"], 1)
        self.assertEqual(row["secondary_points_estimate"], 1)
        self.assertTrue(row["rate_limited"])
        self.assertEqual(row["headers"]["retry-after"], "120")
        self.assertEqual(row["headers"]["x-ratelimit-resource"], "graphql")
        self.assertEqual(row["error_types"], ["RATE_LIMITED"])
        self.assertNotIn("secret", json.dumps(rows))
        self.assertNotIn("private-owner", json.dumps(rows))

    def test_real_gh_graphql_query_and_variables_formatter(self):
        rows = []
        parser = observe.Traffic({}, rows.append)
        wire = b'* Request at time\n* Request to https://api.github.com/graphql\n> POST /graphql HTTP/1.1\n> X-Gh-Cache-Ttl: 24h0m0s\n\nGraphQL query:\nfragment pr on PullRequest {number}\nquery PullRequestSearch($q:String!){search(query:$q){nodes{...pr}}}\nGraphQL variables: {"q":"secret search terms"}\n\n< HTTP/2.0 200 OK\n< Content-Type: application/json\n< Date: Mon, 09 Nov 2026 20:00:00 GMT\n< X-Github-Request-Id: fixed-id\n\n{"data":{}}\n* Request took 123.456ms\n'
        for line in wire.splitlines(keepends=True):
            self.assertTrue(parser.feed(line))
        self.assertEqual(rows[0]["operation"], "PullRequestSearch")
        self.assertEqual(rows[0]["duration_ms"], 123.456)
        self.assertTrue(rows[0]["cached_response"])
        self.assertEqual(rows[0]["request_cache_ttl"], "24h0m0s")
        self.assertNotIn("secret", json.dumps(rows))

    def test_colored_debug_is_parsed_without_changing_stderr(self):
        rows = []
        parser = observe.Traffic({}, rows.append)
        wire = b'* Request at time\n* Request to https://api.github.com/graphql\n> POST /graphql HTTP/1.1\n\n{\n  \x1b[1;34m"query"\x1b[m: \x1b[32m"query Find {viewer{id}}"\x1b[m\n}\n< HTTP/2.0 200 OK\n< Content-Type: application/json\n\n{"data":{"rateLimit":{"cost":3}}}\n* Request took 1ms\n'
        for line in wire.splitlines(keepends=True):
            self.assertTrue(parser.feed(line))
        self.assertEqual(rows[0]["operation"], "Find")
        self.assertEqual(rows[0]["primary_cost"], 3)

    def test_endpoint_authority_preserves_nondefault_ports(self):
        self.assertEqual(observe.safe_endpoint("https://ghe.example:8443/api/graphql?query=secret"), {"host": "ghe.example:8443", "path": "/api/graphql"})
        self.assertEqual(observe.safe_endpoint("https://ghe.example:443/api/graphql"), {"host": "ghe.example", "path": "/api/graphql"})
        self.assertEqual(observe.safe_endpoint("https://api.internal:8443/graphql")["host"], "api.internal:8443")
        self.assertEqual(observe.safe_endpoint("https://api.github.com/graphql")["host"], "github.com")

    def test_endpoint_discards_credentials(self):
        cases = [
            ("https://user:secret@ghe.example:8443/api/graphql?token=secret", "ghe.example:8443"),
            ("https://user:secret@api.github.com:443/graphql", "github.com"),
            ("https://user:secret@[::1]:8443/graphql", "[::1]:8443"),
        ]
        for target, authority in cases:
            with self.subTest(target=target):
                endpoint = observe.safe_endpoint(target)
                self.assertEqual(endpoint["host"], authority)
                self.assertNotIn("secret", json.dumps(endpoint))
                self.assertNotIn("user", json.dumps(endpoint))

    def test_anonymous_query_groups_argument_values(self):
        first = observe.operation('{repository(owner:"one", name:"two"){pullRequests(first:100){totalCount}}}')
        second = observe.operation('{ repository(owner:"three", name:"four") { pullRequests(first:20) {totalCount} } }')
        self.assertEqual(first["shape"], second["shape"])
        self.assertEqual(first["operation"], "anonymous")

    def test_mutation_estimate_is_explicit_and_cost_unknown(self):
        rows = self.parse({"query": "mutation AddComment($input:AddCommentInput!){addComment(input:$input){clientMutationId}}"}, {"data": {}})
        self.assertEqual(rows[0]["secondary_points_estimate"], 5)
        self.assertIsNone(rows[0]["primary_cost"])

    def test_stderr_outside_http_debug_is_preserved(self):
        parser = observe.Traffic({}, lambda row: None)
        self.assertFalse(parser.feed(b"gh: permission denied (HTTP 403)\n"))
        self.assertFalse(parser.feed(b"a tool diagnostic without a newline"))
        self.assertTrue(parser.feed(b"[git remote -v]\n"))

    def test_raw_graphql_aliases_are_filtered_until_variables_marker(self):
        rows = []
        parser = observe.Traffic({}, rows.append)
        wire = b'* Request at time\n* Request to https://api.github.com/graphql\n> POST /graphql HTTP/1.1\n\nGraphQL query:\nquery Example {\ngh: repository(owner:"private-owner",name:"private-repo"){id}\nGraphQL: viewer{login}\n}\nGraphQL variables: {}\n'
        for line in wire.splitlines(keepends=True):
            self.assertTrue(parser.feed(line))
        self.assertFalse(parser.feed(b"gh: connection reset by peer\n"))
        parser.finish(incomplete=True)
        self.assertNotIn("private-owner", json.dumps(rows))
        self.assertNotIn("private-repo", json.dumps(rows))

    def test_literal_framing_ignores_escaped_block_quotes_and_spoofed_boundaries(self):
        framing = observe.TraceFraming()
        prefix = b'* Request at time\n* Request to https://api.github.com/graphql\n> POST /graphql HTTP/1.1\n\nGraphQL query:\n'
        for line in prefix.splitlines(keepends=True):
            framing.classify(line)
        query = 'query Example { repository(owner:"""\nGraphQL variables: {"not":"a boundary"}\ngh: private-query-value\n< HTTP/2.0 599 private-value\n* Request took 1ms\n* Request at fake\n* Request to https://private-query-host/private-path\n\\"""\nGraphQL: private-value\n""",name:"repo"){id} # """ comment\n}\n'
        for line in query.encode().splitlines(keepends=True):
            self.assertEqual(framing.classify(line), (True, True))
        self.assertEqual(framing.classify(b"GraphQL variables: {}\n"), (True, True))
        self.assertEqual(framing.classify(b"gh: connection reset by peer\n"), (False, False))

    def test_non_json_response_body_is_opaque_even_when_it_spoofs_markers(self):
        framing = observe.TraceFraming()
        wire = b'* Request at time\n* Request to https://api.github.com/zen\n> GET /zen HTTP/1.1\n\n< HTTP/2.0 200 OK\n< Content-Type: text/plain\n\n'
        for line in wire.splitlines(keepends=True):
            framing.classify(line)
        for line in (b"gh: PRIVATE_RESPONSE_BODY\n", b"* Request took 1ms\n", b"gh: PRIVATE_AFTER_FOOTER\n", b"* Request at fake\n"):
            self.assertEqual(framing.classify(line), (True, True))

    def test_transport_diagnostic_inside_truncated_debug_is_preserved(self):
        parser = observe.Traffic({}, lambda row: None)
        for line in (b"* Request at time\n", b"* Request to https://api.github.com/graphql\n", b"> POST /graphql HTTP/1.1\n", b"\n", b'{"query":"{viewer{id}}"}\n'):
            self.assertTrue(parser.feed(line))
        self.assertFalse(parser.feed(b"gh: connection reset by peer\n"))
        self.assertFalse(parser.feed(b"GraphQL: request failed\n"))
        self.assertTrue(parser.feed(b'< Authorization: secret-header\n'))

    def test_body_memory_bounded_and_incomplete_record(self):
        rows = []
        parser = observe.Traffic({}, rows.append)
        parser.feed(b"* Request at timestamp\n")
        parser.feed(b"* Request to https://api.github.com/graphql?token=secret\n")
        parser.feed(b"\n")
        for _ in range(8):
            parser.feed(b"x" * (observe.MAX_BODY // 2))
            self.assertLessEqual(len(parser.body), observe.MAX_BODY)
        parser.finish(incomplete=True)
        self.assertTrue(rows[0]["incomplete"])
        self.assertEqual(rows[0]["path"], "/graphql")
        self.assertNotIn("secret", json.dumps(rows))


class ReportTests(unittest.TestCase):
    def test_cached_and_duplicate_debug_exchanges_are_not_new_requests(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            logroot = root / "cagents/gh-observe"
            logroot.mkdir(parents=True)
            base = {"event": "request", "command": "pr list", "host": "api.github.com", "path": "/graphql", "status": 200, "incomplete": False, "operation": "PullRequestSearch", "primary_cost": 2, "secondary_points_estimate": 1}
            rows = [
                {**base, "timestamp": "2026-11-09T23:30:00+00:00", "headers": {"x-github-request-id": "before-window"}},
                {**base, "timestamp": "2026-11-09T23:31:00+00:00", "headers": {"x-github-request-id": "fresh"}},
                {**base, "timestamp": "2026-11-09T23:31:01+00:00", "headers": {"x-github-request-id": "fresh"}},
                {**base, "timestamp": "2026-11-09T23:31:02+00:00", "headers": {"x-github-request-id": "before-window"}},
                {**base, "timestamp": "2026-11-09T23:31:03+00:00", "headers": {"x-github-request-id": "old-cached"}, "cached_response": True},
            ]
            (logroot / "2026-11-09.jsonl").write_text("".join(json.dumps(row) + "\n" for row in rows))
            output = io.StringIO()
            with patch.dict(os.environ, {"XDG_STATE_HOME": directory}), contextlib.redirect_stdout(output):
                observe.report(SimpleNamespace(since="2026-11-09T23:31:00+00:00", until="2026-11-09T23:32:00+00:00"))
            result = json.loads(output.getvalue())
            self.assertEqual(result["network_requests"], 1)
            self.assertEqual(result["logical_exchanges"], 4)
            self.assertEqual(result["reused_response_ids"], 2)
            self.assertEqual(result["cached_by_date_and_ttl"], 1)
            self.assertEqual(result["operations"][0]["primary_cost_sum"], 2)
            self.assertEqual(result["calls_per_minute"][0]["network_count"], 1)
            self.assertEqual(result["calls_per_minute"][0]["logical_count"], 4)

    def test_invocation_only_counts_are_separate_from_http_subset(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory) / "cagents/gh-observe"
            root.mkdir(parents=True)
            caller = {"comm": "codex", "pid": 42, "start_ticks": "100"}
            rows = [
                {"event": "invocation", "timestamp": "2026-11-09T23:31:00+00:00", "command": "pr view", "caller": caller, "detailed": False},
                {"event": "invocation", "timestamp": "2026-11-09T23:31:05+00:00", "command": "pr view", "caller": caller, "detailed": False},
                {"event": "invocation", "timestamp": "2026-11-09T23:31:10+00:00", "command": "api graphql", "caller": caller, "detailed": True, "operation": "Example"},
                {"event": "request", "timestamp": "2026-11-09T23:31:11+00:00", "command": "api graphql", "caller": caller, "host": "github.com", "path": "/graphql", "operation": "Example", "status": 200, "headers": {"x-github-request-id": "one-request"}, "secondary_points_estimate": 1},
            ]
            (root / "2026-11-09.jsonl").write_text("".join(json.dumps(row) + "\n" for row in rows))
            output = io.StringIO()
            with patch.dict(os.environ, {"XDG_STATE_HOME": directory}), contextlib.redirect_stdout(output):
                observe.report(SimpleNamespace(since="2026-11-09T23:31:00+00:00", until="2026-11-09T23:32:00+00:00"))
            report = json.loads(output.getvalue())
            self.assertEqual(report["invocations"], 3)
            self.assertEqual(report["invocation_only_count"], 2)
            self.assertEqual(report["network_requests"], 1)
            self.assertEqual(report["logical_exchanges"], 1)
            self.assertEqual({(row["command"], row["operation"]): row["count"] for row in report["invocations_per_minute"]}, {("pr view", "pr view"): 2, ("api graphql", "Example"): 1})
            self.assertIn("does not mean zero network traffic", report["coverage_note"])


class ShimTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.root = Path(self.directory.name)
        self.repo = self.root / "repo"
        self.repo.mkdir()
        self.git(self.repo, "init", "--quiet")
        self.scope = observe.repository_identity(self.repo)
        self.real = self.root / "gh-real"
        self.marker = self.root / "ran.json"
        self.real.write_text(r'''#!/usr/bin/python3
import json,os,sys
assert sys.argv[1:] == json.loads(os.environ['EXPECT_GH_ARGS'])
assert sys.stdin.buffer.read() == b'input bytes\x00'
with open(os.environ['FAKE_GH_MARKER'], 'w') as stream:
 json.dump({'argv':sys.argv[1:],'debug':os.environ.get('GH_DEBUG'),'cwd':os.getcwd()},stream)
os.write(1,b'{"unchanged":"stdout"}\n')
if os.environ.get('GH_DEBUG') == 'api' or '--verbose' in sys.argv:
 target = b'https://[malformed' if os.environ.get('FAKE_GH_BAD_URL') else b'https://api.github.com/graphql'
 debug = b'* Request at time\n* Request to ' + target + b'\n> POST /graphql HTTP/1.1\n> Authorization: secret\n\n{"query":"{viewer{login}}"}\n< HTTP/2.0 403 Forbidden\n< Content-Type: application/json\n< Retry-After: 120\n< X-Ratelimit-Resource: graphql\n\n{"message":"secondary rate limit for secret-user"}\n* Request took 1ms\n'
 if os.environ.get('FAKE_GH_RAW_QUERY'):
  query = os.environ.get('FAKE_GH_QUERY_TEXT', 'query Example {\ngh: repository(owner:"private-owner",name:"private-repo"){id}\nGraphQL: viewer{login}\n}')
  debug = b'* Request at time\n* Request to ' + target + b'\n> POST /graphql HTTP/1.1\n\nGraphQL query:\n' + query.encode() + b'\nGraphQL variables: {}\n'
 if os.environ.get('FAKE_GH_TEXT_RESPONSE'):
  debug = b'* Request at time\n* Request to ' + target + b'\n> GET /zen HTTP/1.1\n\n< HTTP/2.0 200 OK\n< Content-Type: text/plain\n\ngh: PRIVATE_RESPONSE_BODY\n* Request took 1ms\ngh: PRIVATE_AFTER_SPOOFED_FOOTER\n* Request at fake\n* Request to https://private-response-host/private-path\n'
 if os.environ.get('FAKE_GH_TRUNCATED'):
  debug = debug.split(b'* Request took')[0]
 os.write(2,debug)
os.write(2,os.environ.get('FAKE_GH_EXTRA_STDERR','').encode())
os.write(2,os.environ.get('FAKE_GH_DIAGNOSTIC','gh: forbidden\n').encode())
sys.exit(42)
''')
        self.real.chmod(0o755)
        bindir = self.root / "bin"
        bindir.mkdir()
        (bindir / "gh").symlink_to(self.real)
        self.env = {**os.environ, "HOME": str(self.root / "home"), "XDG_STATE_HOME": str(self.root / "state"), "XDG_DATA_HOME": str(self.root / "data"), "FAKE_GH_MARKER": str(self.marker), "PATH": str(bindir) + ":/usr/bin:/bin"}
        for key in ("GH_DEBUG", "DEBUG", "CAGENTS_GH_OBSERVE", "CAGENTS_GH_OBSERVE_REAL"):
            self.env.pop(key, None)
        self.args = ["api", "graphql", "-f", "query={viewer{login}}"]

    def tearDown(self):
        self.directory.cleanup()

    def git(self, directory, *args):
        return subprocess.run(["/usr/bin/git", "-C", str(directory), *args], check=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE)

    def environment(self, changes=None):
        return {**self.env, "EXPECT_GH_ARGS": json.dumps(self.args), **(changes or {})}

    def invoke(self, changes=None, cwd=None, http=True):
        return subprocess.run([str(SCRIPT), "--shim", str(self.real), self.scope, "http" if http else "calls", *self.args], input=b"input bytes\x00", capture_output=True, env=self.environment(changes), cwd=cwd or self.repo, timeout=3)

    def opted_in(self, command=None, changes=None, http=True):
        command = command or ["gh", *self.args]
        profile = ["--http"] if http else []
        return subprocess.run([str(SCRIPT), "run", "--real", str(self.real), "--repository", str(self.repo), *profile, "--", *command], input=b"input bytes\x00", capture_output=True, env=self.environment(changes), cwd=self.repo, timeout=5)

    def rows(self):
        return [json.loads(line) for file in (self.root / "state/cagents/gh-observe").glob("*.jsonl") for line in file.read_text().splitlines()]

    def assert_unchanged(self, result):
        self.assertEqual(result.returncode, 42, result.stderr)
        self.assertEqual(result.stdout, b'{"unchanged":"stdout"}\n')
        self.assertEqual(result.stderr, b"gh: forbidden\n")

    def test_stdio_arguments_exit_and_error_response(self):
        self.assert_unchanged(self.invoke())
        request = next(row for row in self.rows() if row["event"] == "request")
        self.assertEqual(request["status"], 403)
        self.assertTrue(request["rate_limited"])
        self.assertEqual(request["headers"]["retry-after"], "120")
        self.assertNotIn("secret-user", json.dumps(self.rows()))
        self.assertNotIn("Authorization", json.dumps(self.rows()))
        for log in (self.root / "state/cagents/gh-observe").glob("*.jsonl"):
            self.assertEqual(log.stat().st_mode & 0o777, 0o600)

    def test_default_run_preserves_stderr_and_never_injects_debug(self):
        diagnostic = "* Request at ordinary-user-output\nGraphQL: ordinary-user-output\ngh: ordinary-user-output\n"
        result = self.opted_in(changes={"FAKE_GH_EXTRA_STDERR": diagnostic}, http=False)
        self.assertEqual(result.returncode, 42)
        self.assertEqual(result.stdout, b'{"unchanged":"stdout"}\n')
        self.assertEqual(result.stderr, diagnostic.encode() + b"gh: forbidden\n")
        self.assertIsNone(json.loads(self.marker.read_text())["debug"])
        self.assertEqual(len(self.rows()), 1)
        self.assertFalse(self.rows()[0]["detailed"])

    def test_http_flag_explicitly_enriches_known_json_operation(self):
        self.assert_unchanged(self.opted_in(http=True))
        self.assertEqual(json.loads(self.marker.read_text())["debug"], "api")
        request = next(row for row in self.rows() if row["event"] == "request")
        self.assertEqual(request["headers"]["retry-after"], "120")
        self.assertEqual(request["headers"]["x-ratelimit-resource"], "graphql")

    def test_opt_out_executes_without_logging(self):
        self.assert_unchanged(self.invoke({"CAGENTS_GH_OBSERVE": "0"}))
        self.assertFalse(self.rows())
        self.assertIsNone(json.loads(self.marker.read_text())["debug"])

    def test_existing_debug_is_preserved(self):
        result = self.invoke({"GH_DEBUG": "api"})
        self.assertEqual(result.returncode, 42)
        self.assertIn(b"Authorization: secret", result.stderr)
        self.assertFalse(next(row for row in self.rows() if row["event"] == "invocation")["detailed"])

    def test_explicit_verbose_output_is_preserved(self):
        self.args.append("--verbose")
        result = self.invoke()
        self.assertEqual(result.returncode, 42)
        self.assertIn(b"Authorization: secret", result.stderr)
        self.assertFalse(next(row for row in self.rows() if row["event"] == "invocation")["detailed"])

    def test_logging_failure_executes_real_gh_without_debug(self):
        blocked = self.root / "blocked"
        blocked.write_text("not a directory")
        self.assert_unchanged(self.invoke({"XDG_STATE_HOME": str(blocked)}))
        self.assertIsNone(json.loads(self.marker.read_text())["debug"])

    def test_empty_xdg_state_home_does_not_write_logs_into_repository(self):
        self.assert_unchanged(self.invoke({"XDG_STATE_HOME": ""}))
        logs = self.root / "home/.local/state/cagents/gh-observe"
        self.assertTrue(list(logs.glob("*.jsonl")))
        self.assertFalse((self.repo / "cagents").exists())

    def test_fifo_log_failure_cannot_block_command(self):
        logs = self.root / "state/cagents/gh-observe"
        logs.mkdir(parents=True)
        os.mkfifo(logs / (observe.datetime.datetime.now(observe.datetime.timezone.utc).strftime("%Y-%m-%d") + ".jsonl"))
        self.assert_unchanged(self.invoke())
        self.assertIsNone(json.loads(self.marker.read_text())["debug"])

    def test_stale_enabled_cooldown_and_held_locks_cannot_gate_rest(self):
        self.args = ["api", "repos/owner/repo"]
        root = self.root / "state/cagents/gh-observe"
        (root / "coordination").mkdir(parents=True)
        (root / "coordination.enabled").touch()
        key = observe.hashlib.sha256(b"github.com").hexdigest()[:32]
        data = root / "coordination" / (key + ".json")
        contents = json.dumps({"cooldown_until": time.time() + 3600, "next_write": time.time() + 3600})
        data.write_text(contents)
        descriptors = [os.open(root / "coordination" / (key + suffix), os.O_RDWR | os.O_CREAT, 0o600) for suffix in (".lock", ".content")]
        try:
            for descriptor in descriptors:
                fcntl.flock(descriptor, fcntl.LOCK_EX)
            started = time.monotonic()
            self.assert_unchanged(self.opted_in(changes={"CAGENTS_GH_COORDINATE": "1", "CAGENTS_GH_NONBLOCKING": "0"}))
            self.assertLess(time.monotonic() - started, 2)
            self.assertEqual(data.read_text(), contents)
        finally:
            for descriptor in descriptors:
                os.close(descriptor)

    def test_wrong_repository_is_not_observed(self):
        other = self.root / "other"
        other.mkdir()
        self.git(other, "init", "--quiet")
        command = ["/bin/sh", "-c", 'cd "$1"; shift; exec gh "$@"', "shell", str(other), *self.args]
        self.assert_unchanged(self.opted_in(command))
        self.assertFalse(self.rows())
        self.assertIsNone(json.loads(self.marker.read_text())["debug"])

    def test_real_git_worktree_is_observed(self):
        self.git(self.repo, "-c", "user.name=Fixture", "-c", "user.email=fixture@example.invalid", "commit", "--allow-empty", "-m", "fixture", "--quiet")
        worktree = self.repo / ".worktrees/agent"
        self.git(self.repo, "worktree", "add", "--detach", str(worktree), "HEAD")
        command = ["/bin/sh", "-c", 'cd "$1"; shift; exec gh "$@"', "shell", str(worktree), *self.args]
        self.assert_unchanged(self.opted_in(command))
        self.assertTrue(self.rows())

    def test_nested_opt_in_resolves_unwrapped_real_gh(self):
        command = [str(SCRIPT), "run", "--", "gh", *self.args]
        self.assert_unchanged(self.opted_in(command))
        self.assertEqual(sum(row["event"] == "invocation" for row in self.rows()), 1)

    def test_nested_opt_in_resolves_real_after_provider_filters_environment(self):
        other = self.root / "other"
        other.mkdir()
        self.git(other, "init", "--quiet")
        command = ["/bin/sh", "-c", 'cd "$1"; shift; unset CAGENTS_GH_OBSERVE_REAL; exec "$@"', "shell", str(other), str(SCRIPT), "run", "--repository", str(other), "--http", "--", "gh", *self.args]
        self.assert_unchanged(self.opted_in(command))
        self.assertEqual(json.loads(self.marker.read_text())["debug"], "api")
        self.assertTrue(any(row["event"] == "request" for row in self.rows()))
        self.assertTrue((other / ".git/cagents/gh-observe").is_dir())

    def test_path_activation_is_private_and_nothing_global_is_installed(self):
        result = self.opted_in(["/usr/bin/python3", "-c", 'import shutil;print(shutil.which("gh"))'])
        self.assertEqual(result.returncode, 0, result.stderr)
        wrapper = Path(result.stdout.decode().strip())
        self.assertTrue(wrapper.exists())
        self.assertTrue(wrapper.is_relative_to(Path(self.scope) / "cagents/gh-observe"))
        self.assertFalse((self.root / "home/.local/bin/gh").exists())
        self.assertFalse((self.root / "data").exists())

    def test_background_child_can_use_cached_wrapper_after_launcher_exits(self):
        ready = self.root / "background-ready.json"
        release = self.root / "background-release"
        result = self.root / "background-result.json"
        code = "import json,os,shutil,subprocess,time;from pathlib import Path; wrapper=shutil.which('gh');Path(os.environ['READY']).write_text(json.dumps({'pid':os.getpid(),'wrapper':wrapper}));\nwhile not Path(os.environ['RELEASE']).exists():time.sleep(.01)\nr=subprocess.run([wrapper,*json.loads(os.environ['EXPECT_GH_ARGS'])],input=b'input bytes\\x00',capture_output=True);Path(os.environ['RESULT']).write_text(json.dumps({'code':r.returncode,'stdout':r.stdout.decode(),'stderr':r.stderr.decode()}))"
        launcher = "import os,subprocess;subprocess.Popen(['/usr/bin/python3','-c',os.environ['BACKGROUND_CODE']],stdin=subprocess.DEVNULL,stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)"
        env = {"READY": str(ready), "RELEASE": str(release), "RESULT": str(result), "BACKGROUND_CODE": code}
        process = None
        try:
            launched = self.opted_in(["/usr/bin/python3", "-c", launcher], changes=env)
            self.assertEqual(launched.returncode, 0, launched.stderr)
            deadline = time.monotonic() + 3
            while not ready.exists() and time.monotonic() < deadline:
                time.sleep(0.01)
            self.assertTrue(ready.exists())
            process = json.loads(ready.read_text())
            self.assertTrue(Path(process["wrapper"]).exists())
            release.touch()
            deadline = time.monotonic() + 3
            while not result.exists() and time.monotonic() < deadline:
                time.sleep(0.01)
            self.assertTrue(result.exists())
            observed = json.loads(result.read_text())
            self.assertEqual(observed, {"code": 42, "stdout": '{"unchanged":"stdout"}\n', "stderr": "gh: forbidden\n"})
        finally:
            if process is not None:
                try:
                    os.kill(process["pid"], signal.SIGKILL)
                except ProcessLookupError:
                    pass

    def test_parser_setup_failure_executes_real_gh(self):
        class Executed(BaseException):
            pass
        with patch.object(observe, "repository_identity", return_value=self.scope), patch.object(observe, "Traffic", side_effect=RuntimeError("collector setup")), patch.object(observe, "exec_real", side_effect=Executed) as execute:
            with self.assertRaises(Executed):
                observe.shim(str(self.real), self.scope, self.args, http=True)
        execute.assert_called_once_with(str(self.real), self.args)
        self.assertFalse(self.rows())

    def test_scope_or_wrapper_setup_failure_keeps_original_command_environment(self):
        class Executed(BaseException):
            pass
        args = SimpleNamespace(real=str(self.real), repository=str(self.repo), command=[str(self.real), *self.args])
        for failure in ("scope", "wrapper"):
            with self.subTest(failure=failure):
                scopes = None if failure == "scope" else self.scope
                with patch.object(observe, "repository_identity", return_value=scopes), patch.object(observe.tempfile, "TemporaryDirectory", side_effect=OSError("setup unavailable")), patch.object(observe.os, "execvpe", side_effect=Executed) as execute:
                    with self.assertRaises(Executed):
                        observe.run(args)
                execute.assert_called_once_with(str(self.real), [str(self.real), *self.args], os.environ)

    def test_run_exec_preserves_pid_group_and_stop_signals(self):
        for signum in (signal.SIGTERM, signal.SIGHUP):
            with self.subTest(signal=signum):
                code = "import json,os,signal;print(json.dumps({'pid':os.getpid(),'group':os.getpgrp(),'session':os.getsid(0)}),flush=True);signal.pause()"
                process = subprocess.Popen([str(SCRIPT), "run", "--real", str(self.real), "--repository", str(self.repo), "--", "/usr/bin/python3", "-c", code], env=self.environment(), cwd=self.repo, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
                try:
                    identity = json.loads(process.stdout.readline())
                    self.assertEqual(identity["pid"], process.pid)
                    self.assertEqual(identity["group"], os.getpgrp())
                    self.assertEqual(identity["session"], os.getsid(0))
                    process.send_signal(signum)
                    process.communicate(timeout=3)
                    self.assertEqual(process.returncode, -signum)
                finally:
                    if process.poll() is None:
                        process.kill()
                        process.communicate(timeout=3)

    def test_run_exec_preserves_tty_descriptors_and_pid(self):
        ready = self.root / "tty-ready.json"
        code = "import json,os;from pathlib import Path;Path(os.environ['READY']).write_text(json.dumps({'pid':os.getpid(),'group':os.getpgrp(),'ttys':[os.isatty(fd) for fd in (0,1,2)]}));raise SystemExit(42)"
        master, slave = pty.openpty()
        process = subprocess.Popen([str(SCRIPT), "run", "--real", str(self.real), "--repository", str(self.repo), "--", "/usr/bin/python3", "-c", code], env=self.environment({"READY": str(ready)}), cwd=self.repo, stdin=slave, stdout=slave, stderr=slave)
        os.close(slave)
        try:
            self.assertEqual(process.wait(timeout=3), 42)
            identity = json.loads(ready.read_text())
            self.assertEqual(identity, {"pid": process.pid, "group": os.getpgrp(), "ttys": [True, True, True]})
        finally:
            if process.poll() is None:
                process.kill()
                process.wait(timeout=3)
            os.close(master)

    def test_truncated_debug_preserves_diagnostic_even_after_parser_failure(self):
        for malformed in ("", "1"):
            with self.subTest(parser_failure=bool(malformed)):
                result = self.invoke({"FAKE_GH_TRUNCATED": "1", "FAKE_GH_BAD_URL": malformed, "FAKE_GH_DIAGNOSTIC": "gh: connection reset by peer\n"})
                self.assertEqual(result.returncode, 42)
                self.assertEqual(result.stdout, b'{"unchanged":"stdout"}\n')
                self.assertEqual(result.stderr, b"gh: connection reset by peer\n")
                self.assertNotIn(b"secret", result.stderr)

    def test_raw_graphql_aliases_never_leak_when_parser_has_failed(self):
        for malformed in ("", "1"):
            with self.subTest(parser_failure=bool(malformed)):
                result = self.invoke({"FAKE_GH_RAW_QUERY": "1", "FAKE_GH_BAD_URL": malformed, "FAKE_GH_DIAGNOSTIC": "gh: connection reset by peer\n"})
                self.assertEqual(result.returncode, 42)
                self.assertEqual(result.stdout, b'{"unchanged":"stdout"}\n')
                self.assertEqual(result.stderr, b"gh: connection reset by peer\n")
                self.assertNotIn("private-owner", json.dumps(self.rows()))
                self.assertNotIn("private-repo", json.dumps(self.rows()))

    def test_block_string_markers_never_release_private_query_text(self):
        query = 'query Example { repository(owner:"""\nGraphQL variables: {"not":"a boundary"}\ngh: private-query-value\n< HTTP/2.0 599 private-value\n* Request took 1ms\n* Request at fake\n* Request to https://private-query-host/private-path\n\\"""\nGraphQL: private-value\n""",name:"repo"){id} # """ comment\n}'
        for malformed in ("", "1"):
            with self.subTest(parser_failure=bool(malformed)):
                result = self.invoke({"FAKE_GH_RAW_QUERY": "1", "FAKE_GH_BAD_URL": malformed, "FAKE_GH_QUERY_TEXT": query, "FAKE_GH_DIAGNOSTIC": "gh: connection reset by peer\n"})
                self.assertEqual(result.returncode, 42)
                self.assertEqual(result.stdout, b'{"unchanged":"stdout"}\n')
                self.assertEqual(result.stderr, b"gh: connection reset by peer\n")
                self.assertNotIn("private-query", json.dumps(self.rows()))
                self.assertNotIn("private-path", json.dumps(self.rows()))

    def test_partial_raw_query_dump_is_filtered_conservatively(self):
        query = 'query Example { repository(owner:"""\nGraphQL variables: {}\ngh: private-query-value\n* Request took 1ms\n* Request at fake'
        for malformed in ("", "1"):
            with self.subTest(parser_failure=bool(malformed)):
                result = self.invoke({"FAKE_GH_RAW_QUERY": "1", "FAKE_GH_BAD_URL": malformed, "FAKE_GH_QUERY_TEXT": query, "FAKE_GH_DIAGNOSTIC": "gh: connection reset by peer\n"})
                self.assertEqual(result.returncode, 42)
                self.assertEqual(result.stdout, b'{"unchanged":"stdout"}\n')
                self.assertEqual(result.stderr, b"")
                self.assertNotIn("private-query", json.dumps(self.rows()))

    def test_supplied_unfinished_query_preserves_original_diagnostics_without_debug(self):
        self.args = ["api", "graphql", "-f", 'query=query Example {repository(owner:"""unterminated']
        self.assert_unchanged(self.invoke())
        self.assertIsNone(json.loads(self.marker.read_text())["debug"])
        self.assertFalse(next(row for row in self.rows() if row["event"] == "invocation")["detailed"])

    def test_raw_and_unknown_commands_are_invocation_only(self):
        commands = [
            ["api", "zen"], ["api", "repos/owner/repo"], ["pr", "diff", "1"],
            ["run", "view", "1", "--log"], ["custom-extension", "anything"],
            ["api", "graphql", "--input", "-"],
        ]
        for args in commands:
            with self.subTest(command=args):
                self.args = args
                self.assert_unchanged(self.invoke({"FAKE_GH_TEXT_RESPONSE": "1"}))
                self.assertIsNone(json.loads(self.marker.read_text())["debug"])

    def test_unexpected_text_response_never_leaks_in_normal_or_failed_parser(self):
        for malformed in ("", "1"):
            with self.subTest(parser_failure=bool(malformed)):
                result = self.invoke({"FAKE_GH_TEXT_RESPONSE": "1", "FAKE_GH_BAD_URL": malformed})
                self.assertEqual(result.returncode, 42)
                self.assertEqual(result.stdout, b'{"unchanged":"stdout"}\n')
                # An unexpected opaque response has no trustworthy footer.
                self.assertEqual(result.stderr, b"")
                self.assertNotIn("PRIVATE_RESPONSE", json.dumps(self.rows()))
                self.assertNotIn("private-response-host", json.dumps(self.rows()))

    def test_parser_failure_after_start_never_retries_or_leaks_debug(self):
        self.assert_unchanged(self.invoke({"FAKE_GH_BAD_URL": "1"}))
        self.assertEqual(json.loads(self.marker.read_text())["argv"], self.args)


if __name__ == "__main__":
    unittest.main()
