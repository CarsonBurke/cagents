#!/usr/bin/python3
"""No-network coverage for the gh shim's transparency and safe telemetry."""
import contextlib
import importlib.machinery
import io
import importlib.util
import json
import os
from pathlib import Path
import signal
import subprocess
import tempfile
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
        wire += f"< HTTP/2.0 {status} Response\n"
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
        wire = b'* Request at time\n* Request to https://api.github.com/graphql\n> POST /graphql HTTP/1.1\n> X-Gh-Cache-Ttl: 24h0m0s\n\nGraphQL query:\nfragment pr on PullRequest {number}\nquery PullRequestSearch($q:String!){search(query:$q){nodes{...pr}}}\nGraphQL variables: {"q":"secret search terms"}\n\n< HTTP/2.0 200 OK\n< Date: Mon, 09 Nov 2026 20:00:00 GMT\n< X-Github-Request-Id: fixed-id\n\n{"data":{}}\n* Request took 123.456ms\n'
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
        wire = b'* Request at time\n* Request to https://api.github.com/graphql\n> POST /graphql HTTP/1.1\n\n{\n  \x1b[1;34m"query"\x1b[m: \x1b[32m"query Find {viewer{id}}"\x1b[m\n}\n< HTTP/2.0 200 OK\n\n{"data":{"rateLimit":{"cost":3}}}\n* Request took 1ms\n'
        for line in wire.splitlines(keepends=True):
            self.assertTrue(parser.feed(line))
        self.assertEqual(rows[0]["operation"], "Find")
        self.assertEqual(rows[0]["primary_cost"], 3)

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


class ShimTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.root = Path(self.directory.name)
        self.real = self.root / "gh-real"
        self.real.write_text('''#!/usr/bin/python3
import json,os,sys
assert [arg for arg in sys.argv[1:] if arg != '--verbose'] == ['api', 'graphql', '-f', 'query={viewer{login}}']
assert sys.stdin.buffer.read() == b'input bytes\\x00'
os.write(1, b'{"unchanged":"stdout"}\\n')
if os.environ.get('GH_DEBUG') == 'api' or '--verbose' in sys.argv:
 os.write(2, b'* Request at time\\n* Request to https://api.github.com/graphql\\n> POST /graphql HTTP/1.1\\n> Authorization: secret\\n\\n{"query":"{viewer{login}}"}\\n< HTTP/2.0 403 Forbidden\\n< Retry-After: 120\\n< X-Ratelimit-Resource: graphql\\n\\n{"message":"secondary rate limit for secret-user"}\\n* Request took 1ms\\n')
os.write(2, b'gh: forbidden\\n')
sys.exit(42)
''')
        self.real.chmod(0o755)
        self.env = {**os.environ, "XDG_STATE_HOME": str(self.root / "state"), "XDG_DATA_HOME": str(self.root / "data")}
        self.env.pop("GH_DEBUG", None)
        self.env.pop("DEBUG", None)
        self.args = ["api", "graphql", "-f", "query={viewer{login}}"]

    def tearDown(self):
        self.directory.cleanup()

    def invoke(self, env=None):
        return subprocess.run([str(SCRIPT), "--shim", str(self.real), *self.args], input=b"input bytes\x00", capture_output=True, env=env or self.env)

    def rows(self):
        return [json.loads(line) for file in (self.root / "state/cagents/gh-observe").glob("*.jsonl") for line in file.read_text().splitlines()]

    def test_stdio_arguments_exit_and_error_response(self):
        result = self.invoke()
        self.assertEqual(result.returncode, 42)
        self.assertEqual(result.stdout, b'{"unchanged":"stdout"}\n')
        self.assertEqual(result.stderr, b"gh: forbidden\n")
        rows = self.rows()
        request = next(row for row in rows if row["event"] == "request")
        self.assertEqual(request["status"], 403)
        self.assertTrue(request["rate_limited"])
        self.assertEqual(request["headers"]["retry-after"], "120")
        self.assertNotIn("secret-user", json.dumps(rows))
        self.assertNotIn("Authorization", json.dumps(rows))
        for log in (self.root / "state/cagents/gh-observe").glob("*.jsonl"):
            self.assertEqual(log.stat().st_mode & 0o777, 0o600)

    def test_opt_out_executes_without_logging(self):
        result = self.invoke({**self.env, "CAGENTS_GH_OBSERVE": "0"})
        self.assertEqual(result.returncode, 42)
        self.assertEqual(result.stderr, b"gh: forbidden\n")
        self.assertFalse(self.rows())

    def test_existing_debug_is_preserved(self):
        result = self.invoke({**self.env, "GH_DEBUG": "api"})
        self.assertEqual(result.returncode, 42)
        self.assertIn(b"Authorization: secret", result.stderr)
        self.assertFalse(next(row for row in self.rows() if row["event"] == "invocation")["detailed"])

    def test_explicit_verbose_output_is_preserved(self):
        self.args.append("--verbose")
        result = self.invoke()
        self.assertEqual(result.returncode, 42)
        self.assertIn(b"Authorization: secret", result.stderr)
        self.assertFalse(next(row for row in self.rows() if row["event"] == "invocation")["detailed"])

    def test_logging_failure_does_not_break_command(self):
        blocked = self.root / "blocked"
        blocked.write_text("not a directory")
        result = self.invoke({**self.env, "XDG_STATE_HOME": str(blocked)})
        self.assertEqual(result.returncode, 42)
        self.assertEqual(result.stderr, b"gh: forbidden\n")

    def test_nonblocking_cooldown_exits_before_real_gh(self):
        state = Path(self.env["XDG_STATE_HOME"]) / "cagents/gh-observe"
        state.mkdir(parents=True)
        (state / "coordination.enabled").touch()
        guard = observe.coordinator()
        with guard.locked(state, "github.com") as data:
            data["cooldown_until"] = __import__("time").time() + 3600
        result = self.invoke({**self.env, "CAGENTS_GH_COORDINATE": "1", "CAGENTS_GH_NONBLOCKING": "1"})
        self.assertEqual(result.returncode, 75)
        self.assertEqual(result.stdout, b"")
        self.assertTrue(result.stderr.startswith(b"cagents-gh: deferred "))
        self.assertIn(b"\nRetry-After: ", result.stderr)
        self.assertNotIn(b"gh: forbidden", result.stderr)
        self.assertTrue(next(row for row in self.rows() if row["event"] == "invocation")["deferred"])

    def test_installer_keeps_runtime_after_checkout_is_removed(self):
        checkout = self.root / "checkout"
        checkout.mkdir()
        source = checkout / "gh-observe"
        source.write_bytes(SCRIPT.read_bytes())
        source.chmod(0o755)
        (checkout / "gh-coordinate.py").write_bytes((SCRIPT.parent / "gh-coordinate.py").read_bytes())
        bindir = self.root / "bin"
        result = subprocess.run([str(source), "install", "--directory", str(bindir), "--real", str(self.real)], capture_output=True, env=self.env)
        self.assertEqual(result.returncode, 0, result.stderr)
        source.unlink()
        result = subprocess.run([str(bindir / "gh"), *self.args], input=b"input bytes\x00", capture_output=True, env=self.env)
        self.assertEqual(result.returncode, 42)
        self.assertEqual(result.stderr, b"gh: forbidden\n")
        self.assertTrue(self.rows())

    def test_installer_refuses_existing_command(self):
        bindir = self.root / "bin"
        bindir.mkdir()
        executable = bindir / "gh"
        executable.write_text("keep this file")
        result = subprocess.run([str(SCRIPT), "install", "--directory", str(bindir), "--real", str(self.real)], capture_output=True, env=self.env)
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(executable.read_text(), "keep this file")


if __name__ == "__main__":
    unittest.main()
