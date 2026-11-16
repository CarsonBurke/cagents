#!/usr/bin/python3
"""Contracts for scripts/output-tokens, with a fake uv that counts words; no network or tiktoken."""
import contextlib
import importlib.machinery
import importlib.util
import io
import json
import os
from pathlib import Path
import sys
import tempfile
import unittest
from unittest import mock

ROOT = Path(__file__).resolve().parents[1]
loader = importlib.machinery.SourceFileLoader("output_tokens", str(ROOT / "scripts/output-tokens"))
spec = importlib.util.spec_from_loader(loader.name, loader)
tokens = importlib.util.module_from_spec(spec)
loader.exec_module(tokens)

# Stands in for `uv run ... python3 -I -c COUNTER`: records its arguments and
# environment, fails while FAIL_OFFLINE names an offline run, and otherwise
# counts each text's whitespace-separated words.
FAKE_UV = f'''#!{sys.executable}
import json, os, sys
from pathlib import Path
request = json.load(sys.stdin)
record = Path(os.environ["FAKE_UV_RECORD"])
with record.open("a") as out:
    out.write(json.dumps({{"arguments": sys.argv[1:], "cache": os.environ.get("TIKTOKEN_CACHE_DIR"),
                          "encoding": request["encoding"]}}) + "\\n")
if "--offline" in sys.argv and os.environ.get("FAIL_OFFLINE"):
    print("mise WARN  shim noise", file=sys.stderr)
    print("  cause: Because tiktoken was not found in the cache and you require tiktoken==0.14.0, ...", file=sys.stderr)
    print("hint: Packages were unavailable because the network was disabled.", file=sys.stderr)
    sys.exit(2)
if os.environ.get("FAIL_ALWAYS"):
    print("Traceback (most recent call last):", file=sys.stderr)
    print('  File "<string>", line 4, in <module>', file=sys.stderr)
    print("    encoding = tiktoken.get_encoding(request['encoding'])", file=sys.stderr)
    print("ValueError: Unknown encoding bogus.", file=sys.stderr)
    sys.exit(1)
assert sys.argv[sys.argv.index("-c") + 1].strip().startswith("import json, sys, tiktoken")
print(json.dumps([len(text.split()) for text in request["texts"]]))
'''


class OutputTokens(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory(prefix="cagents-output-tokens-")
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        bin_directory = self.root / "bin"
        bin_directory.mkdir()
        uv = bin_directory / "uv"
        uv.write_text(FAKE_UV)
        uv.chmod(0o755)
        self.record = self.root / "uv-calls.jsonl"
        environment = {"PATH": f"{bin_directory}{os.pathsep}{os.environ.get('PATH', '')}",
                       "FAKE_UV_RECORD": str(self.record), "XDG_CACHE_HOME": str(self.root / "cache")}
        patches = [mock.patch.dict(os.environ, environment),
                   mock.patch.object(tokens, "count_in_process", return_value=None)]
        for patch in patches:
            patch.start()
            self.addCleanup(patch.stop)
        os.environ.pop("TIKTOKEN_CACHE_DIR", None)
        os.environ.pop("FAIL_OFFLINE", None)
        os.environ.pop("FAIL_ALWAYS", None)

    def run_tool(self, *arguments, stdin=b"", merged=False):
        stdout = io.TextIOWrapper(io.BytesIO())
        stderr = stdout if merged else io.TextIOWrapper(io.BytesIO())
        with mock.patch.object(sys, "stdin", io.TextIOWrapper(io.BytesIO(stdin))), \
                mock.patch.object(sys, "stdout", stdout), contextlib.redirect_stderr(stderr):
            try:
                status = tokens.main(list(arguments))
            except SystemExit as exit:
                status = exit.code
        stdout.flush()
        stderr.flush()
        return status, stdout.buffer.getvalue().decode(), "" if merged else stderr.buffer.getvalue().decode()

    def calls(self):
        return [json.loads(line) for line in self.record.read_text().splitlines()]

    def test_a_command_reports_its_total_and_exit_status(self):
        status, out, err = self.run_tool("--", "sh", "-c", "echo one two three; exit 3")
        self.assertEqual((status, err), (3, ""))
        self.assertEqual(out, "3 tokens, 14 bytes, exit 3\n")

    def test_stderr_is_counted_and_split_out_only_when_present(self):
        status, out, _ = self.run_tool("--", "sh", "-c", "echo a b; echo c d e >&2")
        self.assertEqual(status, 0)
        self.assertEqual(out, "5 tokens, 10 bytes (stdout 2/4, stderr 3/6), exit 0\n")

    def test_json_reports_each_stream(self):
        status, out, _ = self.run_tool("--json", "--", "sh", "-c", "echo a b; echo c >&2")
        self.assertEqual(status, 0)
        self.assertEqual(json.loads(out), {
            "encoding": "o200k_base", "tokens": 3, "bytes": 6, "exit": 0,
            "streams": {"stdout": {"tokens": 2, "bytes": 4}, "stderr": {"tokens": 1, "bytes": 2}}})

    def test_standard_input_is_counted_without_a_command(self):
        status, out, _ = self.run_tool(stdin=b"four words of text\n")
        self.assertEqual((status, out), (0, "4 tokens, 19 bytes\n"))
        status, out, _ = self.run_tool("--json", stdin=b"")
        self.assertEqual(json.loads(out), {"encoding": "o200k_base", "tokens": 0, "bytes": 0})

    def test_show_passes_output_through_and_reports_on_stderr(self):
        status, out, err = self.run_tool("--show", "--", "sh", "-c", "echo kept; echo noted >&2")
        self.assertEqual(status, 0)
        self.assertEqual(out, "kept\n")
        self.assertEqual(err, "noted\n2 tokens, 11 bytes (stdout 1/5, stderr 1/6), exit 0\n")

    def test_show_streams_output_live_in_the_order_it_arrives(self):
        script = "echo out1; sleep 0.2; echo err1 >&2; sleep 0.2; echo out2"
        status, out, _ = self.run_tool("--show", "--", "sh", "-c", script, merged=True)
        self.assertEqual(status, 0)
        self.assertEqual(out.splitlines()[:3], ["out1", "err1", "out2"])

    def test_another_encoding_is_named(self):
        _, out, _ = self.run_tool("--encoding", "cl100k_base", "--", "echo", "x")
        self.assertEqual(out, "1 tokens, 2 bytes [cl100k_base], exit 0\n")
        self.assertEqual(self.calls()[0]["encoding"], "cl100k_base")

    def test_uv_runs_offline_first_with_a_pinned_tiktoken_and_cached_vocabulary(self):
        self.run_tool("--", "echo", "x")
        calls = self.calls()
        self.assertEqual(len(calls), 1)
        arguments = calls[0]["arguments"]
        self.assertEqual(arguments[:2], ["run", "--offline"])
        self.assertIn(f"tiktoken=={tokens.TIKTOKEN_VERSION}", arguments)
        self.assertEqual(calls[0]["cache"], str(self.root / "cache/tiktoken"))

    def test_a_cold_cache_falls_back_to_one_online_run(self):
        os.environ["FAIL_OFFLINE"] = "1"
        os.environ["TIKTOKEN_CACHE_DIR"] = str(self.root / "vocabulary")
        status, out, _ = self.run_tool("--", "echo", "x y")
        self.assertEqual((status, out), (0, "2 tokens, 4 bytes, exit 0\n"))
        calls = self.calls()
        self.assertEqual(["--offline" in call["arguments"] for call in calls], [True, False])
        self.assertEqual({call["cache"] for call in calls}, {str(self.root / "vocabulary")})

    def test_a_failed_count_keeps_the_commands_status_and_is_not_retried_online(self):
        os.environ["FAIL_ALWAYS"] = "1"
        for command, expected in ((["true"], 0), (["sh", "-c", "exit 3"], 3)):
            with self.subTest(command=command):
                self.record.unlink(missing_ok=True)
                status, out, err = self.run_tool("--show", "--", *command)
                self.assertEqual((status, out), (expected, ""))
                self.assertEqual(err, f"output-tokens: could not run tiktoken {tokens.TIKTOKEN_VERSION} "
                                      "through uv: ValueError: Unknown encoding bogus.\n")
                self.assertEqual(len(self.calls()), 1, "only a cold cache goes online")
        status, _, _ = self.run_tool(stdin=b"x")
        self.assertEqual(status, tokens.FAILED)

    def test_a_cold_cache_failure_reports_what_uv_said_without_shim_noise(self):
        os.environ["FAIL_OFFLINE"] = "1"
        os.environ["FAIL_ALWAYS"] = "1"
        _, _, err = self.run_tool(stdin=b"x")
        self.assertEqual(len(self.calls()), 2)
        self.assertNotIn("mise WARN", err)

    def test_missing_uv_and_tiktoken_is_explained(self):
        os.environ["PATH"] = str(self.root / "empty")
        status, _, err = self.run_tool(stdin=b"x")
        self.assertEqual(status, tokens.FAILED)
        self.assertIn("install uv or tiktoken", err)

    def test_an_unrunnable_command_and_usage_errors(self):
        status, _, err = self.run_tool("--", str(self.root / "missing"))
        self.assertEqual(status, 127)
        self.assertIn("cannot run", err)
        for arguments in (("--",), ("--show",)):
            with self.subTest(arguments=arguments):
                self.assertEqual(self.run_tool(*arguments)[0], tokens.USAGE)

    def test_a_signalled_command_reports_as_the_shell_would(self):
        _, out, _ = self.run_tool("--", "sh", "-c", "kill -TERM $$")
        self.assertTrue(out.endswith(", exit 143\n"), out)

    def test_undecodable_bytes_are_counted_as_replacements(self):
        self.assertEqual(tokens.decode(b"ok \xff"), "ok �")


@unittest.skipUnless(importlib.util.find_spec("tiktoken"), "tiktoken is not importable")
class RealTokenizer(unittest.TestCase):
    def test_o200k_counts_ordinary_text_and_special_token_spellings(self):
        self.assertEqual(tokens.count_tokens("o200k_base", ["hello world", "<|endoftext|>"])[0], 2)
        self.assertGreater(tokens.count_tokens("o200k_base", ["<|endoftext|>"])[0], 1)


if __name__ == "__main__":
    unittest.main()
