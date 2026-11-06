#!/usr/bin/env python3
import os
from pathlib import Path
import shutil
import signal
import subprocess
import sys
import tempfile
import textwrap
import time
import unittest

ROOT = Path(__file__).resolve().parents[1]
TOOL = ROOT / "scripts/memory-capped"
CAPPED = "CAGENTS_MEMORY_CAPPED"


def real_scopes_work():
    if not shutil.which("systemd-run"):
        return False
    report = 'cat "/sys/fs/cgroup$(sed -n "s/^0:://p" /proc/self/cgroup)/memory.max"'
    found = subprocess.run(["systemd-run", "--user", "--scope", "--quiet", "--collect",
                            "--property=MemoryMax=67108864", "/bin/sh", "-c", report],
                           stdin=subprocess.DEVNULL, capture_output=True, text=True)
    return found.returncode == 0 and found.stdout.strip() == "67108864"


class WithoutAScope(unittest.TestCase):
    """A host with no user manager, through a stand-in systemd-run that fails."""

    def setUp(self):
        temporary = tempfile.TemporaryDirectory(prefix="memory-capped-")
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        stand_in = self.root / "systemd-run"
        stand_in.write_text(f'#!/bin/sh\necho called >>"{self.root}/scope"\nexit 1\n')
        stand_in.chmod(0o755)
        self.env = {key: value for key, value in os.environ.items()
                    if not key.startswith("CAGENTS_MEMORY_")}
        self.env["PATH"] = f"{self.root}:{self.env['PATH']}"

    def run_tool(self, *command, **env):
        return subprocess.run([str(TOOL), *command], env=self.env | env, capture_output=True, text=True)

    def test_the_command_runs_anyway_and_says_it_has_no_limit(self):
        result = self.run_tool("sh", "-c", "echo out; exit 7")
        self.assertEqual((result.returncode, result.stdout), (7, "out\n"))
        self.assertIn("running without a limit", result.stderr)

    def test_zero_turns_the_limit_off_without_looking_for_a_scope(self):
        result = self.run_tool("sh", "-c", "exit 7", CAGENTS_MEMORY_MAX="0")
        self.assertEqual((result.returncode, result.stderr), (7, ""))
        self.assertFalse((self.root / "scope").exists())

    def test_a_command_already_under_a_limit_keeps_it(self):
        result = self.run_tool("sh", "-c", "exit 7", **{CAPPED: "outer.scope"})
        self.assertEqual((result.returncode, result.stderr), (7, ""))
        self.assertFalse((self.root / "scope").exists())

    def test_a_bad_size_is_refused_before_running_anything(self):
        for setting in ("CAGENTS_MEMORY_MAX", "CAGENTS_MEMORY_SWAP_MAX"):
            for value in ("lots", "-1", "1.5G", "16g"):
                with self.subTest(setting=setting, value=value):
                    marker = self.root / "ran"
                    result = self.run_tool("touch", str(marker), **{setting: value})
                    self.assertEqual(result.returncode, 2, result.stderr)
                    self.assertIn(setting, result.stderr)
                    self.assertFalse(marker.exists())

    def test_no_command_is_a_usage_error(self):
        result = self.run_tool()
        self.assertEqual(result.returncode, 2)
        self.assertIn("usage:", result.stderr)


@unittest.skipUnless(real_scopes_work(), "needs a user manager with the memory controller")
class InARealScope(unittest.TestCase):
    def setUp(self):
        self.env = {key: value for key, value in os.environ.items()
                    if not key.startswith("CAGENTS_MEMORY_")}
        self.env |= {"CAGENTS_MEMORY_MAX": "256M", "CAGENTS_MEMORY_SWAP_MAX": "0"}
        temporary = tempfile.TemporaryDirectory(prefix="memory-capped-")
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)

    def run_tool(self, *command, **options):
        return subprocess.run([str(TOOL), *command], env=self.env, capture_output=True, text=True,
                              timeout=120, **options)

    def wait_for(self, predicate, what, seconds=20):
        deadline = time.monotonic() + seconds
        while not predicate():
            self.assertLess(time.monotonic(), deadline, what)
            time.sleep(0.02)

    def test_a_runaway_is_stopped_and_named_with_its_children(self):
        survivor = self.root / "survivor"
        unit = self.root / "unit"
        script = textwrap.dedent(f"""
            import os, pathlib, subprocess
            pathlib.Path("{unit}").write_text(os.environ["{CAPPED}"])
            child = subprocess.Popen(["sh", "-c", 'echo $$ >{survivor}; exec sleep 300'])
            # A ceiling, so that a mutant without the limit fails the test
            # instead of filling the host's memory and swap.
            chunks = [bytearray(b"x") * (16 * 2**20) for _ in range(64)]
        """)
        started = time.monotonic()
        result = self.run_tool(sys.executable, "-I", "-c", script)
        self.assertEqual(result.returncode, 137, result.stderr)
        self.assertIn("went over its limit of 256 MiB", result.stderr)
        self.assertLess(time.monotonic() - started, 60)
        pid = int(survivor.read_text())
        self.wait_for(lambda: not Path(f"/proc/{pid}").exists(), "a child outlived the scope")
        state = subprocess.run(["systemctl", "--user", "is-failed", unit.read_text()],
                               capture_output=True, text=True)
        self.assertNotEqual(state.stdout.strip(), "failed", "the failed unit was left behind")

    def test_memory_under_the_limit_is_undisturbed(self):
        script = "data = bytearray(b'x') * (64 * 2**20); print(len(data))"
        result = self.run_tool(sys.executable, "-I", "-c", script)
        self.assertEqual((result.returncode, result.stdout), (0, f"{64 * 2**20}\n"), result.stderr)

    def test_status_streams_and_input_pass_through(self):
        result = self.run_tool("sh", "-c", "cat; echo err >&2; exit 9", input="in\n")
        self.assertEqual((result.returncode, result.stdout, result.stderr), (9, "in\n", "err\n"))

    def test_the_command_says_it_is_limited_and_leaves_no_failed_unit(self):
        result = self.run_tool("sh", "-c", f'printf %s "${CAPPED}"; exit 3')
        self.assertEqual(result.returncode, 3)
        unit = result.stdout
        self.assertRegex(unit, r"^cagents-capped-.*\.scope$")
        state = subprocess.run(["systemctl", "--user", "is-failed", unit], capture_output=True, text=True)
        self.assertNotEqual(state.stdout.strip(), "failed")

    def test_interrupting_it_stops_what_ignores_the_signal(self):
        marker = self.root / "pid"
        wrapper = subprocess.Popen(
            [str(TOOL), "sh", "-c", f'sh -c \'trap "" TERM INT HUP; echo $$ >"{marker}"; '
             'while :; do sleep 1; done\' & wait'],
            env=self.env, stderr=subprocess.DEVNULL)
        self.addCleanup(wrapper.wait)
        self.wait_for(lambda: marker.exists() and marker.read_text().strip(), "never started")
        pid = int(marker.read_text())
        self.addCleanup(lambda: subprocess.run(["kill", "-KILL", str(pid)], capture_output=True))
        wrapper.terminate()
        wrapper.wait(20)
        self.wait_for(lambda: not Path(f"/proc/{pid}").exists(), "it outlived the scope")

    def test_a_missing_command_fails_as_it_would_alone(self):
        result = self.run_tool(str(self.root / "absent"))
        self.assertNotEqual(result.returncode, 0)

    def test_every_way_of_ending_the_wrapper_ends_the_command(self):
        for name, number in (("terminate", signal.SIGTERM), ("interrupt", signal.SIGINT),
                             ("hang up", signal.SIGHUP), ("kill", signal.SIGKILL)):
            with self.subTest(signal=name):
                marker = self.root / f"pid-{name}"
                wrapper = subprocess.Popen(
                    [str(TOOL), "sh", "-c", f'echo $$ >"{marker}"; exec sleep 300'],
                    env=self.env, stderr=subprocess.DEVNULL)
                self.addCleanup(wrapper.wait)
                self.wait_for(lambda: marker.exists() and marker.read_text().strip(), "never started")
                pid = int(marker.read_text())
                self.addCleanup(lambda p=pid: subprocess.run(["kill", "-KILL", str(p)],
                                                             capture_output=True))
                wrapper.send_signal(number)
                self.wait_for(lambda: not Path(f"/proc/{pid}").exists(), "the command outlived its wrapper")
                wrapper.wait(10)


if __name__ == "__main__":
    unittest.main()
