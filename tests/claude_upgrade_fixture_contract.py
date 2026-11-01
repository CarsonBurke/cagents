#!/usr/bin/env python3
"""Private offline protocol contracts, not HOST/User/native qualification.

Run with python3 -I -B tests/claude_upgrade_fixture_contract.py -v.
Every case earns settled history from the actual fixture before named resume.
"""
import json
import os
from pathlib import Path
import resource
import shutil
import signal
import stat
import subprocess
import sys
import tempfile
import unittest
import uuid


FIXTURE = Path(__file__).resolve().parent / "fixtures/claude-upgrade-continuity.py"
OUTPUT_LIMIT = 1024 * 1024


def cap_child_files():
    resource.setrlimit(resource.RLIMIT_FSIZE, (OUTPUT_LIMIT, OUTPUT_LIMIT))


class ClaudeUpgradeFixtureContract(unittest.TestCase):
    def setUp(self):
        # Fail before launching if exact-child custody or file caps are absent.
        self.assertTrue(callable(getattr(os, "pidfd_open", None)))
        self.assertTrue(callable(getattr(signal, "pidfd_send_signal", None)))
        probe = os.pidfd_open(os.getpid(), 0)
        os.close(probe)
        hard_limit = resource.getrlimit(resource.RLIMIT_FSIZE)[1]
        self.assertTrue(hard_limit == resource.RLIM_INFINITY or hard_limit >= OUTPUT_LIMIT)
        self.root = Path(tempfile.mkdtemp(prefix="updater1513-", dir="/tmp"))
        self.children = []
        self.passed = False
        self.addCleanup(self.release_root)
        self.assertEqual(stat.S_IMODE(self.root.stat().st_mode), 0o700)
        for relative in (
            "bin", "home", "tmp", "xdg", "xdg/config", "xdg/data",
            "xdg/cache", "xdg/state", "xdg/runtime", "fixture-inputs",
            "fixture-inputs/workload", "fixture-inputs/workload/account-alpha",
            "fixture-inputs/workload/account-beta", "fixture-inputs/workload/project",
        ):
            (self.root / relative).mkdir(mode=0o700)
        self.account = self.root / "fixture-inputs/workload/account-alpha"
        self.beta = self.root / "fixture-inputs/workload/account-beta"
        self.project = self.root / "fixture-inputs/workload/project"
        self.source = FIXTURE.read_bytes()
        self.executable = self.root / "bin/claude"
        with self.executable.open("xb") as destination:
            destination.write(self.source)
        self.executable.chmod(0o700)
        # Overlay only synthetic paths on natural child inheritance.
        self.path_overlays = [
            f"HOME={self.root / 'home'}",
            f"CLAUDE_CONFIG_DIR={self.account}",
            f"TMPDIR={self.root / 'tmp'}",
            f"XDG_CONFIG_HOME={self.root / 'xdg/config'}",
            f"XDG_DATA_HOME={self.root / 'xdg/data'}",
            f"XDG_CACHE_HOME={self.root / 'xdg/cache'}",
            f"XDG_STATE_HOME={self.root / 'xdg/state'}",
            f"XDG_RUNTIME_DIR={self.root / 'xdg/runtime'}",
        ]
        self.thread = str(uuid.uuid4())
        self.prompt = str(uuid.uuid4())
        self.initialize = {
            "type": "control_request", "request_id": "offline-initialize",
            "request": {"subtype": "initialize", "forwardSubagentText": True,
                        "supportedDialogKinds": [], "promptSuggestions": True},
        }
        prompt = {"type": "user", "uuid": self.prompt, "message": {
            "role": "user", "content": [{"type": "text", "text": "LIVE_WORK_1513"}],
        }}
        code, stdout, stderr = self.run_fixture(
            "initial", "--session-id", self.encode(self.initialize) + self.encode(prompt)
        )
        self.assertEqual(code, 0)
        self.assertEqual(stderr, b"")
        frames = [json.loads(line) for line in stdout.splitlines()]
        self.assertEqual([frame["type"] for frame in frames],
                         ["control_response", "system", "user", "command_lifecycle"]
                         + ["assistant"] * 40 + ["result"])
        self.assertEqual(frames[0]["response"], {
            "subtype": "success", "request_id": "offline-initialize", "response": {},
        })
        self.assertEqual(frames[1]["session_id"], self.thread)
        self.assertEqual(frames[2]["uuid"], self.prompt)
        self.assertEqual(frames[-1]["session_id"], self.thread)
        self.assertEqual(frames[-1]["subtype"], "success")
        self.assertIs(frames[-1]["is_error"], False)
        self.assertEqual(self.read_private(self.root / "provider-done"), b"done")
        self.assertEqual(self.read_private(self.root / "prompts"), b"prompt\n")
        self.assertEqual(self.read_private(self.root / "produced"),
                         "".join(f"{index}\n" for index in range(40)).encode())
        histories = list(self.account.rglob("*.jsonl"))
        self.assertEqual(len(histories), 1)
        self.history = histories[0]
        self.assertEqual(self.history.name, self.thread + ".jsonl")
        rows = [json.loads(line) for line in self.read_private(self.history).splitlines()]
        self.assertEqual([row["type"] for row in rows], ["user"] + ["assistant"] * 40 + ["result"])
        self.assertTrue(all(row["sessionId"] == self.thread for row in rows))
        self.assertEqual(rows[-1]["subtype"], "success")
        self.assertIs(rows[-1]["is_error"], False)
        self.assertEqual([row["message"]["content"][0]["text"] for row in rows[1:-1]],
                         [f"FRAME_{index:03}|" for index in range(40)])
        self.marker = self.root / "history-retry-start-failed-once.json"
        self.assertFalse(self.marker.exists())
        self.conserved = {
            path: self.read_private(path) for path in (
                self.history, self.executable, self.root / "provider-done",
                self.root / "prompts", self.root / "produced",
            )
        }
        self.trace_before = self.read_private(self.root / "provider-trace.jsonl")

    @staticmethod
    def encode(frame):
        return (json.dumps(frame) + "\n").encode()

    def read_private(self, path):
        self.assertLessEqual(path.stat().st_size, OUTPUT_LIMIT)
        with path.open("rb") as source:
            data = source.read(OUTPUT_LIMIT + 1)
        self.assertLessEqual(len(data), OUTPUT_LIMIT)
        return data

    def run_fixture(self, phase, argument, payload):
        # This exact fixture has no child creation/network and receives finite
        # input plus EOF. Only its original direct child may be signalled.
        stdout_path, stderr_path = (self.root / f"{phase}.{name}" for name in ("stdout", "stderr"))
        with stdout_path.open("xb") as stdout, stderr_path.open("xb") as stderr:
            os.fchmod(stdout.fileno(), 0o600)
            os.fchmod(stderr.fileno(), 0o600)
            child = subprocess.Popen(
                ["/usr/bin/env", *self.path_overlays, "/usr/bin/python3", "-I", "-B",
                 str(self.executable), argument, self.thread],
                cwd=self.project, env=None, stdin=subprocess.PIPE,
                stdout=stdout, stderr=stderr, bufsize=0, close_fds=True,
                preexec_fn=cap_child_files,
            )
            record = {"child": child, "pidfd": None, "natural": False, "phase": phase}
            self.children.append(record)
            try:
                record["pidfd"] = os.pidfd_open(child.pid, 0)
                self.assertLessEqual(len(payload), os.fpathconf(child.stdin.fileno(), "PC_PIPE_BUF"))
                if payload:
                    self.assertEqual(os.write(child.stdin.fileno(), payload), len(payload))
            finally:
                # EOF releases both the normal frame loop and missing-init case.
                try:
                    child.stdin.close()
                finally:
                    try:
                        child.wait(timeout=10)
                        record["natural"] = True
                    except subprocess.TimeoutExpired:
                        if record["pidfd"] is not None:
                            try:
                                signal.pidfd_send_signal(record["pidfd"], signal.SIGKILL)
                            except ProcessLookupError:
                                pass  # Exit can race the pidfd signal; still reap.
                            finally:
                                child.wait(timeout=3)
                        self.fail(f"{phase}: fixture exceeded natural wait; cleanup is not a pass")
                    finally:
                        if child.returncode is not None and record["pidfd"] is not None:
                            os.close(record["pidfd"])
                            record["pidfd"] = None
        return child.returncode, self.read_private(stdout_path), self.read_private(stderr_path)

    def release_root(self):
        settled = all(record["child"].returncode is not None for record in self.children)
        custody = [(record["phase"], record["child"].returncode, record["natural"])
                   for record in self.children]
        if self.passed and settled:
            shutil.rmtree(self.root)
            print(f"offline fixture custody {self._testMethodName}: {custody}; private root released")
        else:
            print(f"retained offline fixture root {self.root}; custody={custody}", file=sys.stderr)
        self.assertTrue(settled, "original direct child unsettled; private root retained")

    def check_resume(self, payload, diagnostic=None):
        code, stdout, stderr = self.run_fixture("resume", "--resume", payload)
        self.assertEqual(stdout, b"", "first refusal must emit no confirmation or response")
        if diagnostic is None:
            self.assertEqual(code, 75)
            self.assertEqual(stderr, b"")
        else:
            self.assertNotIn(code, (0, 75), "invalid initialize must not reach the exit-75 refusal")
            self.assertIn(diagnostic, stderr)
        metadata = self.marker.lstat()
        self.assertTrue(stat.S_ISREG(metadata.st_mode))
        self.assertEqual((metadata.st_uid, metadata.st_nlink, stat.S_IMODE(metadata.st_mode)),
                         (os.getuid(), 1, 0o600))
        self.assertEqual(self.read_private(self.marker), json.dumps({
            "thread": self.thread, "account": str(self.account),
        }, sort_keys=True).encode())
        trace = self.read_private(self.root / "provider-trace.jsonl")
        self.assertTrue(trace.startswith(self.trace_before))
        added = [json.loads(line) for line in trace[len(self.trace_before):].splitlines()]
        expected_events = ["bootstrap", "fixture-owner"]
        if diagnostic is None:
            expected_events.append("owned-fixture-resume-refused-once")
            self.assertEqual(added[-1]["thread"], self.thread)
        self.assertEqual([entry["event"] for entry in added], expected_events)
        self.assertEqual(added[1]["account"], str(self.account))
        for path, before in self.conserved.items():
            self.assertEqual(self.read_private(path), before)
        self.assertEqual(FIXTURE.read_bytes(), self.source)
        self.assertEqual(list(self.account.rglob("*.jsonl")), [self.history])
        self.assertEqual(list(self.beta.iterdir()), [])
        self.assertEqual([record["natural"] for record in self.children], [True, True])
        self.passed = True

    def test_named_resume_drains_initialize_without_confirmation(self):
        self.check_resume(self.encode(self.initialize))

    def test_eof_is_not_an_initialize(self):
        self.check_resume(b"", b"missing initialize before fixture refusal")

    def test_malformed_json_is_not_an_initialize(self):
        self.check_resume(b"{\n", b"JSONDecodeError")

    def test_wrong_type_is_not_an_initialize(self):
        self.check_resume(self.encode({"type": "user", "request": {"subtype": "initialize"}}),
                          b"AssertionError")

    def test_wrong_subtype_is_not_an_initialize(self):
        self.check_resume(self.encode({"type": "control_request", "request": {"subtype": "interrupt"}}),
                          b"AssertionError")


if __name__ == "__main__":
    unittest.main()
