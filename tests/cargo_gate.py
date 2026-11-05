#!/usr/bin/env python3
"""Exercise held-command lifetime with private locks and owned processes only."""

import ctypes
import errno
import fcntl
import hashlib
import json
import os
from pathlib import Path
import pty
import select
import shutil
import signal
import subprocess
import sys
import tempfile
import time
import unittest


ROOT = Path(__file__).resolve().parents[1]


class CargoGateApplicability(unittest.TestCase):
    """Run CI's exact inline Cargo block against private source-head fixtures."""

    paths = ("scripts/cargo-gate", "scripts/cargo-gate-command.py", "tests/cargo_gate.py")

    @classmethod
    def setUpClass(cls):
        lines = (ROOT / ".github/workflows/script-contracts.yml").read_text().splitlines(keepends=True)
        step = "      - name: Check Cargo gate source and contracts\n"
        starts = [index for index, line in enumerate(lines) if line == step]
        if len(starts) != 1 or lines[starts[0] + 1] != "        run: |\n":
            raise AssertionError("expected exactly one inline Cargo applicability step")
        body = []
        for line in lines[starts[0] + 2:]:
            if line.startswith("          "):
                body.append(line[10:])
            elif not line.strip():
                body.append("\n")
            else:
                break
        cls.run_block = "".join(body)
        if not cls.run_block.startswith("python3 -I -B - <<'PY'\n"):
            raise AssertionError("Cargo applicability must run inline isolated Python")

    def setUp(self):
        temporary = tempfile.TemporaryDirectory(prefix="cargo-contract-applicability-")
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)

    def case(self, name):
        root = self.root / name
        root.mkdir(mode=0o700)
        return root

    def sources(self, root, *, legacy=False, exit_code=0):
        gate, source, fixture = (root / name for name in self.paths)
        gate.parent.mkdir(parents=True)
        gate.write_text("#!/bin/sh\n" + ("" if legacy else "# cargo-gate-command.py\n") + "exit 0\n")
        if legacy:
            return
        source.write_text("fixture supervisor placeholder; never execute this file\n")
        fixture.parent.mkdir()
        fixture.write_text(
            "import json\nfrom pathlib import Path\nimport sys\n"
            "Path('calls.json').write_text(json.dumps({'arguments': sys.argv[1:], "
            "'isolated': sys.flags.isolated, 'bytecode': sys.flags.dont_write_bytecode}))\n"
            f"raise SystemExit({exit_code})\n"
        )

    def invoke(self, root):
        # No Cargo, Git, production script, environment override or global lock
        # is involved. Only the private fixture test file can be executed.
        return subprocess.run(
            ["/bin/sh", "-e", "-c", self.run_block], cwd=root,
            capture_output=True, text=True, timeout=15,
        )

    def assert_hash(self, result, root, name):
        digest = hashlib.sha256((root / name).read_bytes()).hexdigest()
        self.assertIn(f"{digest}  {name}\n", result.stdout)

    def test_old_head_without_gate_files_is_explicitly_not_applicable(self):
        root = self.case("old")
        result = self.invoke(root)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout.splitlines(), [
            "Not applicable: Cargo gate (source absent at this head)",
            "Not applicable: held-command cancellation (source and fixture both absent at this head)",
        ])
        self.assertEqual(list(root.iterdir()), [])

    def test_legacy_gate_is_hashed_and_missing_supervisor_pair_is_not_applicable(self):
        root = self.case("legacy")
        self.sources(root, legacy=True)
        result = self.invoke(root)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assert_hash(result, root, self.paths[0])
        self.assertIn("Not applicable: held-command cancellation", result.stdout)
        self.assertFalse((root / "calls.json").exists())

    def test_complete_source_hashes_all_members_and_runs_the_isolated_fixture(self):
        root = self.case("current")
        self.sources(root)
        result = self.invoke(root)
        self.assertEqual(result.returncode, 0, result.stderr)
        for name in self.paths:
            self.assert_hash(result, root, name)
        self.assertIn("Checking: held-command cancellation\n", result.stdout)
        self.assertEqual(json.loads((root / "calls.json").read_text()), {
            "arguments": ["-v"], "isolated": 1, "bytecode": 1,
        })

    def test_partial_or_inconsistent_current_source_is_refused_before_execution(self):
        for index, missing in enumerate((
            (self.paths[0],), (self.paths[1],), (self.paths[2],), self.paths[1:],
        )):
            with self.subTest(missing=missing):
                root = self.case(f"partial-{index}")
                self.sources(root)
                for name in missing:
                    (root / name).unlink()
                result = self.invoke(root)
                self.assertEqual(result.returncode, 2, result.stderr)
                self.assertIn("Incomplete ", result.stderr)
                self.assertEqual(result.stdout, "")
                self.assertFalse((root / "calls.json").exists())

    def test_fixture_failure_status_is_preserved(self):
        root = self.case("failure")
        self.sources(root, exit_code=19)
        result = self.invoke(root)
        self.assertEqual(result.returncode, 19, result.stderr)
        self.assertTrue((root / "calls.json").exists())

    def test_runner_requiring_an_absent_gate_is_refused(self):
        root = self.case("current-runner-without-gate")
        runner = root / "scripts/run-development"
        runner.parent.mkdir()
        runner.write_text('#!/bin/sh\n"$root/scripts/cargo-gate" build\n')
        result = self.invoke(root)
        self.assertEqual(result.returncode, 2, result.stderr)
        self.assertIn("references missing scripts/cargo-gate", result.stderr)
        self.assertFalse((root / "calls.json").exists())

    def test_legacy_shell_syntax_is_still_required(self):
        root = self.case("bad-syntax")
        self.sources(root, legacy=True)
        (root / self.paths[0]).write_text("#!/bin/sh\nif\n")
        result = self.invoke(root)
        self.assertNotEqual(result.returncode, 0)
        self.assert_hash(result, root, self.paths[0])
        self.assertNotIn("Not applicable: held-command cancellation", result.stdout)

    def test_symlink_dangling_and_nonregular_members_fail_without_execution(self):
        for index, member in enumerate(self.paths):
            for kind in ("symlink", "dangling", "directory"):
                with self.subTest(member=member, kind=kind):
                    root = self.case(f"invalid-{index}-{kind}")
                    self.sources(root)
                    path = root / member
                    contents = path.read_bytes()
                    path.unlink()
                    if kind == "directory":
                        path.mkdir()
                    else:
                        target = root / "private-target"
                        if kind == "symlink":
                            target.write_bytes(contents)
                        path.symlink_to(target)
                    result = self.invoke(root)
                    self.assertEqual(result.returncode, 2, result.stderr)
                    self.assertIn("Invalid contract path:", result.stderr)
                    self.assertFalse((root / "calls.json").exists())

    def test_nonregular_parent_components_fail_closed(self):
        for parent in ("scripts", "tests"):
            for kind in ("file", "symlink"):
                with self.subTest(parent=parent, kind=kind):
                    root = self.case(f"parent-{parent}-{kind}")
                    path = root / parent
                    if kind == "file":
                        path.write_text("non-directory fixture parent\n")
                    else:
                        target = root / "private-directory"
                        target.mkdir()
                        path.symlink_to(target, target_is_directory=True)
                    result = self.invoke(root)
                    self.assertEqual(result.returncode, 2, result.stderr)
                    self.assertIn("Invalid contract path:", result.stderr)
                    self.assertFalse((root / "calls.json").exists())


class HeldCommand(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        if sys.platform != "linux" or not hasattr(os, "pidfd_open"):
            raise unittest.SkipTest("Linux pidfds are required")
        cls.libc = ctypes.CDLL(None, use_errno=True)
        previous = ctypes.c_int()
        if cls.libc.prctl(37, ctypes.byref(previous), 0, 0, 0):
            raise OSError(ctypes.get_errno(), "cannot inspect child subreaper")
        cls.previous_subreaper = previous.value
        if cls.libc.prctl(36, 1, 0, 0, 0):
            raise OSError(ctypes.get_errno(), "cannot own orphaned fixture descendants")

    @classmethod
    def tearDownClass(cls):
        cls.libc.prctl(36, cls.previous_subreaper, 0, 0, 0)

    def setUp(self):
        temporary = tempfile.TemporaryDirectory(prefix="cagents-gate-held-")
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.gate = self.root / "cargo-gate"
        shutil.copy2(ROOT / "scripts/cargo-gate", self.gate)
        helper = ROOT / "scripts/cargo-gate-command.py"
        if helper.exists():
            shutil.copy2(helper, self.root / helper.name)
        self.lock = self.root / "slot.lock"
        self.env = os.environ | {
            "CAGENTS_BUILD_LOCK": str(self.lock),
            "CAGENTS_BUILD_SLOTS": "1",
            "CARGO_BUILD_JOBS": "1",
        }
        self.env.pop("CAGENTS_BUILD_SLOT_HELD", None)
        self.children = []
        self.descendants = []
        self.addCleanup(self.cleanup_processes)

    def cleanup_processes(self):
        # Every pidfd comes from this fixture's private ready receipt. Release
        # graceful handlers first, then kill only the original owned handles.
        (self.root / "release").touch()
        for _pid, pidfd in self.descendants:
            if not select.select([pidfd], [], [], 0)[0]:
                signal.pidfd_send_signal(pidfd, signal.SIGKILL)
        for child in self.children:
            if child.poll() is None:
                child.kill()
            child.wait(timeout=5)
            if child.stdout:
                child.stdout.close()
            if child.stderr:
                child.stderr.close()
        for pid, pidfd in self.descendants:
            try:
                os.waitpid(pid, 0)
            except ChildProcessError:
                pass  # The fixed supervisor already reaped it.
            os.close(pidfd)

    def wait_for(self, predicate, label, timeout=5):
        deadline = time.monotonic() + timeout
        while not predicate():
            if time.monotonic() >= deadline:
                self.fail(f"timed out waiting for {label}")
            time.sleep(0.01)

    def slot_is_free(self):
        with self.lock.open("a") as slot:
            try:
                fcntl.flock(slot, fcntl.LOCK_EX | fcntl.LOCK_NB)
                return True
            except BlockingIOError:
                return False

    def assert_reaped(self, pidfd):
        self.assertTrue(select.select([pidfd], [], [], 0)[0])
        pid = next(pid for pid, owned in self.descendants if owned == pidfd)
        try:
            os.waitpid(pid, os.WNOHANG)
        except ChildProcessError:
            return
        self.fail("supervisor exited without reaping its descendant")

    def launch_descendant(self, *, detached=False, background=False, resistant_primary=False):
        # The shell is the supported `exec COMMAND`; its child deliberately
        # takes time to finish after TERM so an early slot release is observable.
        worker = self.root / "worker.py"
        worker.write_text("""
import json, os, pathlib, signal, sys, time
root = pathlib.Path(sys.argv[1])
if sys.argv[2] == 'detached':
    os.setsid()
def stop(_signal, _frame):
    (root / 'terminated').touch()
    while not (root / 'release').exists():
        time.sleep(0.01)
    sys.exit(0)
signal.signal(signal.SIGTERM, stop)
(root / 'ready.tmp').write_text(json.dumps({'pid': os.getpid()}))
(root / 'ready.tmp').rename(root / 'ready')
while True:
    time.sleep(1)
""")
        command = '"$1" "$2" "$3" "$4" & wait'
        if background:
            command = '"$1" "$2" "$3" "$4" & while [ ! -f "$3/ready" ]; do sleep 0.01; done'
        if resistant_primary:
            command = 'trap \'touch "$3/parent-terminated"\' TERM; "$1" "$2" "$3" "$4" & while :; do wait; done'
        child = subprocess.Popen(
            [str(self.gate), "exec", "/bin/sh", "-c", command, "fixture",
             sys.executable, str(worker), str(self.root),
             "detached" if detached else "ordinary"],
            env=self.env, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
            start_new_session=True,
        )
        self.children.append(child)
        self.wait_for(lambda: (self.root / "ready").exists(), "descendant ready")
        pid = json.loads((self.root / "ready").read_text())["pid"]
        pidfd = os.pidfd_open(pid)
        self.descendants.append((pid, pidfd))
        if not background:
            self.assertFalse(self.slot_is_free(), "live command owns its slot")
        return child, pidfd

    def test_cancellation_keeps_slot_until_descendant_exits(self):
        child, pidfd = self.launch_descendant()
        child.send_signal(signal.SIGTERM)
        self.wait_for(
            lambda: (self.root / "terminated").exists() or child.poll() is not None,
            "forwarded cancellation",
        )
        self.assertIsNone(child.poll(), "gate exited with an owned descendant alive")
        self.assertFalse(self.slot_is_free(), "cleanup must retain the slot")
        self.assertFalse(select.select([pidfd], [], [], 0)[0])
        (self.root / "release").touch()
        self.assertEqual(child.wait(timeout=5), 143)
        self.assert_reaped(pidfd)
        self.assertTrue(self.slot_is_free())

    def test_cancellation_reaps_a_descendant_that_left_the_command_group(self):
        child, pidfd = self.launch_descendant(detached=True)
        child.send_signal(signal.SIGTERM)
        self.assertEqual(child.wait(timeout=6), 143)
        self.assert_reaped(pidfd)
        self.assertTrue(self.slot_is_free())

    def test_success_leaves_background_cache_services_outside_the_slot(self):
        child, pidfd = self.launch_descendant(background=True)
        self.assertEqual(child.wait(timeout=5), 0)
        self.assertFalse(select.select([pidfd], [], [], 0)[0])
        self.assertTrue(self.slot_is_free())

    def test_cancellation_drains_children_after_a_resistant_parent_is_killed(self):
        child, pidfd = self.launch_descendant(resistant_primary=True)
        child.send_signal(signal.SIGTERM)
        self.wait_for((self.root / "parent-terminated").exists, "parent cancellation handler")
        self.assertIsNone(child.poll())
        self.assertFalse(self.slot_is_free())
        self.assertFalse(select.select([pidfd], [], [], 0)[0])
        self.assertEqual(child.wait(timeout=6), 143)
        self.assert_reaped(pidfd)
        self.assertTrue(self.slot_is_free())

    def run_gate(self, *arguments, **options):
        return subprocess.run(
            [str(self.gate), *arguments], env=self.env,
            capture_output=True, text=True, timeout=5, **options,
        )

    def test_exit_status_and_missing_command_are_preserved(self):
        for status in (0, 17):
            with self.subTest(status=status):
                result = self.run_gate("exec", "/bin/sh", "-c", f"exit {status}")
                self.assertEqual(result.returncode, status, result.stderr)
                self.assertTrue(self.slot_is_free())
        result = self.run_gate("exec", str(self.root / "missing"))
        self.assertEqual(result.returncode, 127, result.stderr)
        self.assertTrue(self.slot_is_free())

    def test_nested_exec_uses_the_held_slot(self):
        result = self.run_gate("exec", str(self.gate), "exec", "/bin/sh", "-c", "exit 19")
        self.assertEqual(result.returncode, 19, result.stderr)
        self.assertTrue(self.slot_is_free())

    def test_stdin_and_caller_descriptor_reach_the_command(self):
        with (self.root / "caller-output").open("w") as output:
            result = self.run_gate(
                "exec", sys.executable, "-c",
                "import os,sys; os.write(int(sys.argv[1]), sys.stdin.buffer.read())",
                str(output.fileno()), pass_fds=(output.fileno(),), input="caller data\n",
            )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual((self.root / "caller-output").read_text(), "caller data\n")

    def test_exec_preserves_the_controlling_terminal(self):
        child, terminal = pty.fork()
        if child == 0:
            os.execve(self.gate, [
                str(self.gate), "exec", sys.executable, "-c",
                "open('/dev/tty').close(); print('controlling-terminal-ok')",
            ], self.env)
        pidfd = os.pidfd_open(child)
        output = bytearray()
        try:
            deadline = time.monotonic() + 5
            while True:
                remaining = deadline - time.monotonic()
                self.assertGreater(remaining, 0, "terminal command did not finish")
                ready, _, _ = select.select([terminal], [], [], remaining)
                self.assertTrue(ready, "terminal command did not finish")
                try:
                    chunk = os.read(terminal, 4096)
                except OSError as error:
                    if error.errno != errno.EIO:
                        raise
                    break
                if not chunk:
                    break
                output.extend(chunk)
            _, status = os.waitpid(child, 0)
            self.assertEqual(os.waitstatus_to_exitcode(status), 0, output.decode())
            self.assertIn(b"controlling-terminal-ok", output)
        finally:
            if not select.select([pidfd], [], [], 0)[0]:
                # pty.fork made this exact unreaped fixture child its own
                # session/group leader; no other command shares that group.
                os.killpg(child, signal.SIGKILL)
                os.waitpid(child, 0)
            os.close(pidfd)
            os.close(terminal)

    def test_cancelled_build_does_not_run_the_second_cargo_phase(self):
        # Real gate, stub Cargo: cancellation remains cancellation even when
        # the build command handles TERM and returns success.
        binary = self.root / "bin"
        binary.mkdir()
        cargo = binary / "cargo"
        cargo.write_text(f"""#!{sys.executable}
import pathlib, signal, sys, time
root = pathlib.Path({str(self.root)!r})
with (root / 'cargo-calls').open('a') as calls:
    calls.write(' '.join(sys.argv[1:]) + '\\n')
signal.signal(signal.SIGTERM, lambda *_: sys.exit(0))
(root / 'build-ready').touch()
while True:
    time.sleep(1)
""")
        cargo.chmod(0o755)
        self.env["PATH"] = f"{binary}:{self.env['PATH']}"
        for command, number in (("test", signal.SIGTERM), ("bench", signal.SIGINT), ("run", signal.SIGHUP)):
            with self.subTest(command=command, signal=number):
                ready = self.root / "build-ready"
                calls = self.root / "cargo-calls"
                child = subprocess.Popen(
                    [str(self.gate), command, "--locked", "--", "program-argument"],
                    env=self.env, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                    start_new_session=True,
                )
                self.children.append(child)
                self.wait_for(ready.exists, "stub Cargo build")
                child.send_signal(number)
                self.assertEqual(child.wait(timeout=5), 128 + number)
                expected = "build --locked" if command == "run" else f"{command} --no-run --locked"
                self.assertEqual(calls.read_text().splitlines(), [expected])
                self.assertTrue(self.slot_is_free())
                ready.unlink()
                calls.unlink()

    def test_successful_split_preserves_cargo_and_program_arguments(self):
        binary = self.root / "bin"
        binary.mkdir()
        cargo = binary / "cargo"
        cargo.write_text(f"""#!{sys.executable}
import json, pathlib, sys
with pathlib.Path({str(self.root / 'cargo-calls')!r}).open('a') as calls:
    calls.write(json.dumps(sys.argv[1:]) + '\\n')
""")
        cargo.chmod(0o755)
        self.env["PATH"] = f"{binary}:{self.env['PATH']}"
        for command in ("test", "bench", "run"):
            with self.subTest(command=command):
                result = self.run_gate(command, "--locked", "--", "argument with spaces")
                self.assertEqual(result.returncode, 0, result.stderr)
                first = ["build", "--locked"] if command == "run" else [command, "--no-run", "--locked"]
                calls = self.root / "cargo-calls"
                self.assertEqual(
                    [json.loads(line) for line in calls.read_text().splitlines()],
                    [first, [command, "--locked", "--", "argument with spaces"]],
                )
                calls.unlink()



class MemoryAdmission(unittest.TestCase):
    """Two slots, one held by a gated build that records its expected cost."""

    def setUp(self):
        temporary = tempfile.TemporaryDirectory(prefix="cagents-gate-memory-")
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.gate = self.root / "cargo-gate"
        shutil.copy2(ROOT / "scripts/cargo-gate", self.gate)
        shutil.copy2(ROOT / "scripts/cargo-gate-command.py", self.root / "cargo-gate-command.py")
        self.env = {
            key: value for key, value in os.environ.items()
            if not key.startswith("CAGENTS_BUILD_") and key != "CARGO_BUILD_JOBS"
        } | {"CAGENTS_BUILD_LOCK": str(self.root / "slot.lock"), "CAGENTS_BUILD_SLOTS": "2"}

    def hold(self, cost):
        holder = subprocess.Popen(
            [str(self.gate), "exec", "sh", "-c", f"touch {self.root}/held; "
             f"while [ ! -e {self.root}/release ]; do sleep 0.05; done"],
            env=self.env | {"CAGENTS_BUILD_COST_GIB": str(cost)}, cwd=self.root,
            stderr=subprocess.DEVNULL,
        )
        self.addCleanup(holder.wait, 10)
        self.holder = holder
        self.addCleanup((self.root / "release").touch)
        deadline = time.monotonic() + 10
        while not (self.root / "held").exists():
            self.assertLess(time.monotonic(), deadline, "holder never admitted")
            time.sleep(0.02)
        return holder

    def build(self, cost, timeout):
        return subprocess.run(
            [str(self.gate), "exec", "sh", "-c", 'printf "%s" "$CARGO_BUILD_JOBS"'],
            env=self.env | {"CAGENTS_BUILD_COST_GIB": str(cost)}, cwd=self.root,
            capture_output=True, text=True, timeout=timeout,
        )

    def test_a_build_that_fits_starts_beside_a_running_one(self):
        self.hold(1)
        result = self.build(1, timeout=10)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertNotIn("waiting", result.stderr)
        cpus = os.cpu_count()
        self.assertEqual(int(result.stdout), max(4, min(8, cpus // 2)))
        # Released, the slot no longer prices its last holder.
        (self.root / "release").touch()
        self.holder.wait(timeout=10)
        self.assertFalse(any(self.root.glob("slot.lock*.cost")))

    def test_a_build_that_does_not_fit_waits_for_the_running_one(self):
        self.hold(1)
        with self.assertRaises(subprocess.TimeoutExpired):
            self.build(10**6, timeout=2)
        (self.root / "release").touch()
        result = self.build(10**6, timeout=10)
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_a_running_builds_expected_peak_is_reserved(self):
        # The holder has claimed almost none of an expected peak larger than
        # any host's memory, so even a small newcomer must wait.
        self.hold(10**6)
        with self.assertRaises(subprocess.TimeoutExpired):
            self.build(1, timeout=2)

    def test_a_lone_build_gets_half_the_cpus(self):
        result = self.build(1, timeout=10)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(int(result.stdout), max(4, min(8, os.cpu_count() // 2)))

    def test_a_slot_held_by_a_plain_flock_is_priced_not_fatal(self):
        holder = subprocess.Popen(
            ["flock", self.root / "slot.lock", "sh", "-c",
             f"touch {self.root}/held; while [ ! -e {self.root}/release ]; do sleep 0.05; done"])
        self.addCleanup(holder.wait, 10)
        self.addCleanup((self.root / "release").touch)
        while not (self.root / "held").exists():
            time.sleep(0.02)
        # Priced as the largest build, so whether a small build fits depends
        # on this host; either way the gate admits or keeps waiting.
        waiter = subprocess.Popen(
            [str(self.gate), "exec", "true"], env=self.env | {"CAGENTS_BUILD_COST_GIB": "1"},
            cwd=self.root, stderr=subprocess.PIPE, text=True)
        try:
            status = waiter.wait(timeout=2)
        except subprocess.TimeoutExpired:
            waiter.kill()
            status = waiter.wait()
            self.assertNotIn("unbound", waiter.stderr.read())
        else:
            self.assertEqual(status, 0, waiter.stderr.read())
        waiter.stderr.close()

    def test_an_invalid_cost_is_refused(self):
        result = self.build("08", timeout=10)
        self.assertEqual(result.returncode, 2)
        self.assertIn("CAGENTS_BUILD_COST_GIB", result.stderr)

if __name__ == "__main__":
    unittest.main()
