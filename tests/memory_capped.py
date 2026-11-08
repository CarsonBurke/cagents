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
LEAF = "sed -n 's|.*/||p' /proc/self/cgroup"


def real_scopes_work():
    if not shutil.which("systemd-run"):
        return False
    report = 'cat "/sys/fs/cgroup$(sed -n "s/^0:://p" /proc/self/cgroup)/memory.max"'
    found = subprocess.run(["systemd-run", "--user", "--scope", "--quiet", "--collect",
                            "--property=MemoryMax=67108864", "/bin/sh", "-c", report],
                           stdin=subprocess.DEVNULL, capture_output=True, text=True)
    return found.returncode == 0 and found.stdout.strip() == "67108864"


class UnderAMovedHome(unittest.TestCase):
    """A mise shim for python3 re-executes itself forever under a fixture's HOME,
    and the app and sessions start here from one, so this can't go through PATH."""

    def test_it_starts_without_consulting_path(self):
        with tempfile.TemporaryDirectory() as directory:
            shim = Path(directory) / "python3"
            shim.write_text('#!/bin/sh\nexec "$0" "$@"\n')
            shim.chmod(0o755)
            result = subprocess.run([TOOL, "--help"], capture_output=True, text=True, timeout=20,
                                    env=os.environ | {"PATH": f"{directory}:{os.defpath}"})
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("usage:", result.stdout)


class WithoutAScope(unittest.TestCase):
    """A host with no user manager, through a stand-in systemd-run that fails."""

    def test_a_name_needs_a_command(self):
        result = subprocess.run([TOOL, "--name", "the app"], capture_output=True, text=True)
        self.assertEqual(result.returncode, 2)
        self.assertIn("usage:", result.stderr)

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

    def assert_not_left_failed(self, unit):
        state = subprocess.run(["systemctl", "--user", "is-failed", unit], capture_output=True, text=True)
        self.assertNotEqual(state.stdout.strip(), "failed", "the failed unit was left behind")

    def test_a_runaway_is_stopped_and_named_with_its_children(self):
        survivor = self.root / "survivor"
        unit = self.root / "unit"
        script = textwrap.dedent(f"""
            import pathlib, subprocess
            cgroup = pathlib.Path("/proc/self/cgroup").read_text().strip()
            pathlib.Path("{unit}").write_text(cgroup.rsplit("/", 1)[1])
            child = subprocess.Popen(["sh", "-c", 'echo $$ >{survivor}; exec sleep 300'])
            # A ceiling, so that a mutant without the limit fails the test
            # instead of filling the host's memory and swap.
            chunks = [bytearray(b"x") * (16 * 2**20) for _ in range(64)]
        """)
        started = time.monotonic()
        result = self.run_tool(sys.executable, "-I", "-c", script)
        self.assertEqual(result.returncode, 137, result.stderr)
        self.assertIn(f"stopped {Path(sys.executable).name} because it and the processes "
                      "it started went over the limit of 256 MiB", result.stderr)
        self.assertIn("not a crash", result.stderr)
        self.assertIn("CAGENTS_MEMORY_MAX=512M", result.stderr)
        self.assertLess(time.monotonic() - started, 60)
        pid = int(survivor.read_text())
        self.wait_for(lambda: not Path(f"/proc/{pid}").exists(), "a child outlived the scope")
        self.assert_not_left_failed(unit.read_text())

    def test_memory_under_the_limit_is_undisturbed(self):
        script = "data = bytearray(b'x') * (64 * 2**20); print(len(data))"
        result = self.run_tool(sys.executable, "-I", "-c", script)
        self.assertEqual((result.returncode, result.stdout), (0, f"{64 * 2**20}\n"), result.stderr)

    def test_status_streams_and_input_pass_through(self):
        result = self.run_tool("sh", "-c", "cat; echo err >&2; exit 9", input="in\n")
        self.assertEqual((result.returncode, result.stdout, result.stderr), (9, "in\n", "err\n"))

    def test_the_command_says_it_is_limited_and_leaves_no_failed_unit(self):
        result = self.run_tool("sh", "-c", f"{LEAF}; exit 3")
        self.assertEqual(result.returncode, 3)
        unit = result.stdout.strip()
        self.assertRegex(unit, r"^cagents-capped-.*\.scope$")
        state = subprocess.run(["systemctl", "--user", "is-failed", unit], capture_output=True, text=True)
        self.assertNotEqual(state.stdout.strip(), "failed")

    def test_interrupting_it_stops_what_ignores_the_signal(self):
        marker = self.root / "pid"
        unit = self.root / "unit"
        wrapper = subprocess.Popen(
            [str(TOOL), "sh", "-c", f'{LEAF} >"{unit}"; sh -c \'trap "" TERM INT HUP; echo $$ >"{marker}"; '
             'while :; do sleep 1; done\' & wait'],
            env=self.env, stderr=subprocess.PIPE, text=True)
        self.addCleanup(wrapper.communicate)
        self.wait_for(lambda: marker.exists() and marker.read_text().strip(), "never started")
        pid = int(marker.read_text())
        self.addCleanup(lambda: subprocess.run(["kill", "-KILL", str(pid)], capture_output=True))
        wrapper.terminate()
        _, errors = wrapper.communicate(timeout=20)
        self.wait_for(lambda: not Path(f"/proc/{pid}").exists(), "it outlived the scope")
        self.assertNotIn("over the limit", errors)
        self.assert_not_left_failed(unit.read_text().strip())

    def test_an_interrupted_command_ends_a_calling_script_too(self):
        marker = self.root / "pid"
        script = subprocess.Popen(
            ["bash", "-c", f'"{TOOL}" sh -c \'echo $$ >"{marker}"; exec sleep 300\'; echo continued'],
            env=self.env, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, start_new_session=True)
        self.addCleanup(script.communicate)
        self.wait_for(lambda: marker.exists() and marker.read_text().strip(), "never started")
        # As a terminal delivers Ctrl-C: to the whole foreground group.
        os.killpg(script.pid, signal.SIGINT)
        output, errors = script.communicate(timeout=30)
        self.assertEqual((script.returncode, output), (-signal.SIGINT, ""), errors)

    def test_a_stricter_limit_of_the_callers_stays_in_force(self):
        result = subprocess.run(
            ["systemd-run", "--user", "--scope", "--quiet", "--collect", "--property=MemoryMax=134217728",
             "--property=MemorySwapMax=0", str(TOOL), "sh", "-c",
             f'{LEAF}; cat "/sys/fs/cgroup$(sed -n "s/^0:://p" /proc/self/cgroup)/memory.max"'],
            env=self.env, capture_output=True, text=True, timeout=60)
        leaf, limit = result.stdout.split()
        self.assertNotRegex(leaf, r"^cagents-capped-", result.stderr)
        self.assertEqual(limit, "134217728")

    def test_any_size_is_honoured_and_zero_turns_it_off(self):
        page = os.sysconf("SC_PAGE_SIZE")
        self.env["CAGENTS_MEMORY_MAX"] = str(64 * 2**20 + 1)
        report = f'{LEAF}; cat "/sys/fs/cgroup$(sed -n "s/^0:://p" /proc/self/cgroup)/memory.max"'
        result = self.run_tool("sh", "-c", report)
        self.assertNotIn("without a limit", result.stderr)
        leaf, limit = result.stdout.split()
        self.assertRegex(leaf, r"^cagents-capped-")
        self.assertEqual(int(limit), 64 * 2**20 + page)
        for off in ("0", "0G"):
            self.env["CAGENTS_MEMORY_MAX"] = off
            result = self.run_tool("sh", "-c", LEAF)
            self.assertNotRegex(result.stdout, r"^cagents-capped-", off)
            self.assertEqual(result.stderr, "", off)

    def test_a_pipe_reader_going_away_ends_a_command_run_without_a_scope(self):
        self.env["CAGENTS_MEMORY_MAX"] = "0"
        result = subprocess.run(
            ["bash", "-c", f'"{TOOL}" sh -c "while echo x; do :; done" | head -n 1; echo "${{PIPESTATUS[0]}}"'],
            env=self.env, capture_output=True, text=True, timeout=30)
        self.assertEqual(result.stdout.split(), ["x", "141"], result.stderr)

    def test_a_command_in_a_limited_scope_stays_in_it_and_one_outside_gets_its_own(self):
        inner = f'"{TOOL}" sh -c "{LEAF}"'
        result = self.run_tool("sh", "-c", f"{LEAF}; {inner}")
        self.assertEqual(result.returncode, 0, result.stderr)
        outer, nested = result.stdout.split()
        self.assertEqual(nested, outer)
        # A daemon's own scope, started from a limited one, isn't limited.
        escaped = subprocess.run(
            ["systemd-run", "--user", "--scope", "--quiet", "--collect",
             f"--unit=cagentsd-memory-capped-{os.getpid()}.scope", str(TOOL), "sh", "-c", LEAF],
            env=self.env, capture_output=True, text=True, timeout=60)
        self.assertRegex(escaped.stdout.strip(), r"^cagents-capped-.*\.scope$", escaped.stderr)

    def test_a_replaced_runtime_directory_keeps_the_limit_and_the_command_keeps_it(self):
        # As in a fixture or a nested session, where systemd's tools can't find
        # the user manager from these variables.
        replaced = {"XDG_RUNTIME_DIR": str(self.root), "DBUS_SESSION_BUS_ADDRESS": "unix:path=/nonexistent"}
        self.env |= replaced
        result = self.run_tool("sh", "-c", f'{LEAF}; echo "$XDG_RUNTIME_DIR $DBUS_SESSION_BUS_ADDRESS"')
        self.assertEqual(result.returncode, 0, result.stderr)
        leaf, variables = result.stdout.split("\n", 1)
        self.assertRegex(leaf, r"^cagents-capped-.*\.scope$", result.stderr)
        self.assertEqual(variables.strip(), f"{self.root} unix:path=/nonexistent")
        self.env.pop("DBUS_SESSION_BUS_ADDRESS")
        result = self.run_tool("sh", "-c", 'echo "${DBUS_SESSION_BUS_ADDRESS-unset}"')
        self.assertEqual(result.stdout.strip(), "unset", result.stderr)
        ceiling = "chunks = [bytearray(b'x') * (16 * 2**20) for _ in range(64)]"
        result = self.run_tool("--name", "the app", sys.executable, "-I", "-c", ceiling)
        self.assertEqual(result.returncode, 137, result.stderr)
        self.assertIn("stopped the app because", result.stderr)

    def test_a_missing_command_fails_as_it_would_alone(self):
        result = self.run_tool(str(self.root / "absent"))
        self.assertNotEqual(result.returncode, 0)

    def test_the_command_keeps_descriptors_it_inherits(self):
        # A held build slot arrives as one, as it would without this wrapper.
        with open(self.root / "held", "w") as held:
            descriptor = held.fileno()
            result = subprocess.run([str(TOOL), "sh", "-c", f'[ -e /proc/self/fd/{descriptor} ]'],
                                    env=self.env, pass_fds=[descriptor], capture_output=True, text=True,
                                    timeout=60)
        self.assertEqual(result.returncode, 0, result.stderr)

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
