"""Owned Linux pidfd regressions; deterministic hooks are not a historical replay.

Run serially, after qualification admission, with:
    /usr/bin/python3 -I -B tests/qualification/test_process_custody.py -v

Every fixture starts with Custody's empty-child admission, launches only its own
Python worker and pipe-held child, and uses ordinary positive family settlement
before closing the original pidfds. No daemon, display, provider or foreign PID
is selected. The injected metadata errors exercise an observation boundary;
the child's exit and original pidfd readability are real kernel observations.
"""

import errno
import importlib.util
import os
from pathlib import Path
import re
import select
import subprocess
import sys
import tempfile
import time
import unittest
from unittest import mock


MODULE_PATH = Path(__file__).resolve().parents[2] / "scripts/qualification/process_custody.py"
HANDSHAKE_SECONDS = 5

# The parent deliberately leaves its own exited child unreaped until requested.
# That makes positive death and outstanding non-child reaping independently
# observable to Custody; a later parent waitpid leaves its status unavailable to
# Custody rather than donating the worker's known exit status to the inventory.
WORKER = r"""
import os, select, sys
reader, writer = os.pipe()
child = os.fork()
if child == 0:
    os.close(writer)
    os.read(reader, 1)
    os.close(reader)
    os._exit(23)
os.close(reader)
pidfd = os.pidfd_open(child, 0)
os.write(1, ('ready ' + str(child) + '\n').encode())
try:
    for command in sys.stdin.buffer:
        if command == b'exit\n':
            os.write(writer, b'x')
            os.close(writer)
            writer = None
            poller = select.poll()
            poller.register(pidfd, select.POLLIN)
            if not any(flags & select.POLLIN for _, flags in poller.poll(5000)):
                raise RuntimeError('Owned fixture child did not exit within its handshake bound')
            os.write(1, b'dead\n')
        elif command == b'reap\n':
            waited, status = os.waitpid(child, 0)
            if waited != child or os.waitstatus_to_exitcode(status) != 23:
                raise RuntimeError('Unexpected original fixture wait status')
            os.write(1, b'reaped 23\n')
        else:
            raise RuntimeError('Unexpected fixed fixture command')
finally:
    os.close(pidfd)
    if writer is not None:
        os.close(writer)
"""


class OwnedFamily:
    """One test-owned family; no alternate reaper or cleanup success surrogate."""

    def __init__(self, case, module):
        self.case = case
        self.module = module
        self.owner = None
        self.worker = None
        self.directory = None
        self.buffer = b""

    def __enter__(self):
        self.directory = tempfile.TemporaryDirectory(prefix="cagents-owned-pidfd-fixture-")
        try:
            self.owner = self.module.Custody(lambda: False)
            self.worker = self.owner.launch(
                [sys.executable, "-I", "-B", "-c", WORKER],
                self.directory.name,
                "fixture-owned-parent",
                stdin=subprocess.PIPE,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                bufsize=0,
            )
            ready = re.fullmatch(rb"ready ([0-9]+)\n", self.line())
            self.case.assertIsNotNone(ready, "The actual owned worker must publish its child")
            self.child_pid = int(ready[1])
            self.owner.scan()
            self.node = self.owner.nodes[self.child_pid]
            self.case.assertEqual(self.node["ppid"], self.worker.pid)
            self.case.assertEqual(self.node["provenance"], "observed owned descendant")
            self.case.assertFalse(self.owner.exited(self.node))
            self.executable = self.owner.current(self.node, executable=True)["executable"]
            return self
        except BaseException:
            self.close()
            raise

    def line(self):
        deadline = time.monotonic() + HANDSHAKE_SECONDS
        while b"\n" not in self.buffer:
            remaining = deadline - time.monotonic()
            self.case.assertGreater(remaining, 0, "Fixed owned-child handshake timed out")
            poller = select.poll()
            poller.register(self.worker.stdout.fileno(), select.POLLIN | select.POLLHUP)
            self.case.assertTrue(poller.poll(max(1, int(remaining * 1000))))
            block = os.read(self.worker.stdout.fileno(), 4096)
            self.case.assertTrue(block, "Owned worker ended before its fixed handshake")
            self.buffer += block
            self.case.assertLessEqual(len(self.buffer), 4096)
        line, self.buffer = self.buffer.split(b"\n", 1)
        return line + b"\n"

    def command(self, value, expected):
        self.worker.stdin.write(value)
        self.worker.stdin.flush()
        self.case.assertEqual(self.line(), expected)

    def close(self):
        if self.owner is not None:
            reports = []
            final = self.owner.settle_all(
                lambda: None,
                lambda stage, error: reports.append((stage, type(error).__name__, str(error))),
            )
            self.case.assertTrue(all(node["settled"] for node in final["nodes"]))
            descriptors = [node["pidfd"] for node in self.owner.nodes.values() if "pidfd" in node]
            self.owner.close_pidfds()
            for descriptor in descriptors:
                with self.case.assertRaises(OSError) as caught:
                    os.fstat(descriptor)
                self.case.assertEqual(caught.exception.errno, errno.EBADF)
            self.case.assertTrue(all("pidfd" not in node for node in self.owner.nodes.values()))
            if self.worker is not None:
                self.case.assertIsNotNone(self.worker.returncode)
                for stream in (self.worker.stdin, self.worker.stdout, self.worker.stderr):
                    stream.close()
            self.directory.cleanup()
            self.owner = None
            self.case.assertEqual(reports, [], "Positive cleanup must not hide custody errors")
        elif self.directory is not None:
            self.directory.cleanup()

    def __exit__(self, *_error):
        self.close()


def observe_real_polls(before_wait, after_wait=None):
    """Observe requested waits; every readiness result comes from kernel poll."""
    real_poll = select.poll
    observations = []

    class ObservedPoll:
        def __init__(self):
            self.original = real_poll()
            self.descriptors = []

        def register(self, descriptor, events):
            self.descriptors.append(descriptor)
            return self.original.register(descriptor, events)

        def poll(self, timeout=None):
            waiting = timeout is not None and timeout > 0
            descriptors = tuple(self.descriptors)
            if waiting:
                before_wait(descriptors, timeout)
            events = self.original.poll(timeout)
            if waiting:
                observations.append((descriptors, timeout, list(events)))
                if after_wait is not None:
                    after_wait(descriptors, timeout, events)
            return events

    return ObservedPoll, observations


class ProcessCustodyExitTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        # Import occurs only during a separately admitted test execution, never
        # while source preparation parses or compiles this fixture.
        spec = importlib.util.spec_from_file_location("qualified_process_custody", MODULE_PATH)
        cls.module = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(cls.module)

    def test_owned_exit_during_metadata_read_retains_the_original_reaping_obligation(self):
        for missing_errno in (errno.ENOENT, errno.ESRCH):
            with self.subTest(metadata_errno=missing_errno), OwnedFamily(self, self.module) as family:
                owner, node = family.owner, family.node
                descriptor = node["pidfd"]
                current = owner.current
                calls = []

                def exit_between_reads(candidate, executable=False):
                    if candidate is node:
                        calls.append(candidate["pid"])
                        self.assertFalse(owner.exited(candidate))
                        family.command(b"exit\n", b"dead\n")
                        self.assertTrue(owner.exited(candidate), "Original kernel pidfd must prove exit")
                        raise OSError(missing_errno, "Injected pinned-node metadata disappearance")
                    return current(candidate, executable=executable)

                with mock.patch.object(owner, "current", side_effect=exit_between_reads):
                    matches = owner.exact_matches(family.executable)
                self.assertEqual(calls, [node["pid"]])
                self.assertEqual([item[0]["pid"] for item in matches], [family.worker.pid])
                self.assertIs(owner.nodes[node["pid"]], node)
                self.assertEqual(node["pidfd"], descriptor)
                self.assertFalse(node["settled"], "Positive exit cannot replace the outstanding parent reaping")
                self.assertNotIn("exit_code", node)
                self.assertFalse(owner.reap(node), "The original parent still owns its zombie wait status")

                family.command(b"reap\n", b"reaped 23\n")
                self.assertTrue(owner.reap(node))
                self.assertIsNone(node["exit_code"], "Another parent's status is unavailable, never assumed zero")
                self.assertIn("status unavailable", node["exit_status_source"])
                self.assertIs(owner.nodes[node["pid"]], node)
                self.assertEqual(node["pidfd"], descriptor)
                self.assertNotIn(node["pid"], [item[0]["pid"] for item in owner.exact_matches(family.executable)])

    def test_live_missing_metadata_and_permission_errors_remain_fail_closed(self):
        with OwnedFamily(self, self.module) as family:
            owner, node = family.owner, family.node
            current = owner.current
            for number in (errno.ENOENT, errno.ESRCH, errno.EACCES):
                with self.subTest(metadata_errno=number):
                    def missing(candidate, executable=False):
                        if candidate is node:
                            raise OSError(number, "Injected live owned-node metadata failure")
                        return current(candidate, executable=executable)

                    with mock.patch.object(owner, "current", side_effect=missing), \
                            mock.patch.object(owner, "exited", wraps=owner.exited) as exit_checks:
                        with self.assertRaises((OSError, RuntimeError)):
                            owner.exact_matches(family.executable)
                    grace = [call for call in exit_checks.call_args_list
                             if call.args[0] is node and call.kwargs.get("wait_until") is not None]
                    self.assertEqual(len(grace), 0 if number == errno.EACCES else 1)
                    self.assertFalse(owner.exited(node))
                    self.assertFalse(node["settled"])
                    self.assertNotIn("exit_code", node)
                    self.assertIs(owner.nodes[node["pid"]], node)

    def test_cookie_mismatch_and_invalid_original_pidfd_are_not_exit_proof(self):
        with OwnedFamily(self, self.module) as family:
            owner, node = family.owner, family.node
            ticks = node["start_ticks"]
            try:
                node["start_ticks"] = ticks + 1
                with mock.patch.object(owner, "exited", wraps=owner.exited) as exit_checks:
                    with self.assertRaisesRegex(RuntimeError, "incarnation changed"):
                        owner.exact_matches(family.executable)
                self.assertFalse(any(call.kwargs.get("wait_until") is not None
                                     for call in exit_checks.call_args_list))
            finally:
                node["start_ticks"] = ticks
            self.assertFalse(owner.exited(node))

            descriptor = node["pidfd"]
            invalid = os.dup(descriptor)
            os.close(invalid)
            current = owner.current

            def invalidate_at_metadata(candidate, executable=False):
                if candidate is node:
                    candidate["pidfd"] = invalid
                    raise FileNotFoundError(errno.ENOENT, "Injected pinned-node disappearance")
                return current(candidate, executable=executable)

            try:
                with mock.patch.object(owner, "current", side_effect=invalidate_at_metadata):
                    with self.assertRaisesRegex(RuntimeError, "pidfd became invalid"):
                        owner.exact_matches(family.executable)
            finally:
                node["pidfd"] = descriptor
            self.assertFalse(owner.exited(node))
            self.assertFalse(node["settled"])
            self.assertIs(owner.nodes[node["pid"]], node)
            self.assertNotIn("exit_code", node)

    def test_expected_executable_file_failure_is_not_a_node_disappearance(self):
        with OwnedFamily(self, self.module) as family:
            real_stat = os.stat

            def expected_file_missing(path, *args, **kwargs):
                if os.fspath(path) == family.executable:
                    raise FileNotFoundError(errno.ENOENT, "Injected expected executable-file disappearance")
                return real_stat(path, *args, **kwargs)

            with mock.patch.object(self.module.os, "stat", side_effect=expected_file_missing), \
                    mock.patch.object(family.owner, "exited", wraps=family.owner.exited) as exit_checks:
                with self.assertRaisesRegex(FileNotFoundError, "expected executable-file"):
                    family.owner.exact_matches(family.executable)
            self.assertFalse(any(call.kwargs.get("wait_until") is not None
                                 for call in exit_checks.call_args_list))
            self.assertFalse(family.owner.exited(family.node))
            self.assertFalse(family.node["settled"])
            self.assertNotIn("exit_code", family.node)

    def test_owned_metadata_disappearance_waits_only_on_original_pidfds(self):
        for number in (errno.ENOENT, errno.ESRCH):
            with self.subTest(metadata_errno=number), OwnedFamily(self, self.module) as family:
                owner, node = family.owner, family.node
                descriptor = node["pidfd"]
                current = owner.current
                missing = []
                released = []

                def metadata_missing(candidate, executable=False):
                    if candidate is node:
                        self.assertFalse(owner.exited(node))
                        missing.append(node["pid"])
                        raise OSError(number, "Injected pre-exit metadata disappearance")
                    return current(candidate, executable=executable)

                def release_at_bounded_poll(descriptors, timeout):
                    self.assertEqual(descriptors, (descriptor,))
                    self.assertGreater(timeout, 0)
                    self.assertLessEqual(timeout, 100)
                    self.assertEqual(missing, [node["pid"]])
                    self.assertFalse(released, "Only one bounded wait releases this real child")
                    self.assertFalse(owner.exited(node), "Real child is still pipe-held")
                    family.worker.stdin.write(b"exit\n")
                    family.worker.stdin.flush()
                    released.append(descriptor)

                poll_proxy, observations = observe_real_polls(release_at_bounded_poll)
                with mock.patch.object(owner, "current", side_effect=metadata_missing), \
                        mock.patch.object(self.module.select, "poll", poll_proxy):
                    matches = owner.exact_matches(family.executable)
                self.assertEqual(released, [descriptor])
                self.assertEqual(len(observations), 1)
                self.assertTrue(any(flags & select.POLLIN for _, flags in observations[0][2]))
                self.assertEqual([item[0]["pid"] for item in matches], [family.worker.pid])
                self.assertIs(owner.nodes[node["pid"]], node)
                self.assertEqual(node["pidfd"], descriptor)
                self.assertFalse(node["settled"])
                self.assertNotIn("exit_code", node)
                self.assertFalse(owner.reap(node), "Actual parent still owns the zombie status")
                self.assertEqual(family.line(), b"dead\n")
                family.command(b"reap\n", b"reaped 23\n")
                self.assertTrue(owner.reap(node))
                self.assertIsNone(node["exit_code"])
                self.assertIn("status unavailable", node["exit_status_source"])
                self.assertEqual(node["pidfd"], descriptor)

        with self.subTest(shared_budget=True), OwnedFamily(self, self.module) as family:
            owner, node = family.owner, family.node
            worker_node = owner.nodes[family.worker.pid]
            descriptor, worker_descriptor = node["pidfd"], worker_node["pidfd"]
            self.assertEqual(set(owner.nodes), {node["pid"], family.worker.pid})
            # Keep the same real records/fds, but inspect the departing child
            # before its genuinely live parent in this single matching pass.
            owner.nodes[family.worker.pid] = owner.nodes.pop(family.worker.pid)
            current = owner.current
            clock = [1000.0]
            released = []

            def metadata_missing(candidate, executable=False):
                if candidate is node or candidate is worker_node:
                    self.assertFalse(owner.exited(candidate))
                    raise OSError(errno.ENOENT, "Injected original-node metadata disappearance")
                return current(candidate, executable=executable)

            def before_wait(descriptors, timeout):
                self.assertGreater(timeout, 0)
                self.assertLessEqual(timeout, 100)
                if descriptors == (descriptor,):
                    self.assertFalse(released)
                    family.worker.stdin.write(b"exit\n")
                    family.worker.stdin.flush()
                    released.append(descriptor)
                else:
                    self.assertEqual(descriptors, (worker_descriptor,))
                    self.assertEqual(released, [descriptor])
                    self.assertLessEqual(timeout, 11, "The second node cannot receive a new 100 ms budget")

            def after_wait(descriptors, _timeout, events):
                if descriptors == (descriptor,):
                    self.assertTrue(any(flags & select.POLLIN for _, flags in events))
                    # Control only budget arithmetic; readiness and process
                    # exit remain real. No handshake runs under this clock.
                    clock[0] += 0.09
                else:
                    self.assertFalse(events, "The real worker remains alive")

            poll_proxy, observations = observe_real_polls(before_wait, after_wait)
            with mock.patch.object(owner, "current", side_effect=metadata_missing), \
                    mock.patch.object(self.module.select, "poll", poll_proxy), \
                    mock.patch.object(self.module.time, "monotonic", side_effect=lambda: clock[0]):
                with self.assertRaisesRegex(RuntimeError, "metadata vanished without original pidfd exit"):
                    owner.exact_matches(family.executable)
            self.assertEqual([item[0] for item in observations], [(descriptor,), (worker_descriptor,)])
            self.assertIs(owner.nodes[node["pid"]], node)
            self.assertIs(owner.nodes[family.worker.pid], worker_node)
            self.assertEqual((node["pidfd"], worker_node["pidfd"]), (descriptor, worker_descriptor))
            self.assertFalse(node["settled"] or worker_node["settled"])
            self.assertNotIn("exit_code", node)
            self.assertNotIn("exit_code", worker_node)
            self.assertEqual(family.line(), b"dead\n")
            family.command(b"reap\n", b"reaped 23\n")
            self.assertTrue(owner.reap(node))
            self.assertIsNone(node["exit_code"])
            self.assertFalse(owner.exited(worker_node))


if __name__ == "__main__":
    unittest.main()
