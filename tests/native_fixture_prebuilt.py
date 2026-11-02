#!/usr/bin/python3 -I
"""Fixed prebuilt-carrier contracts, using mocks and tiny private files only.

No entry point or authority guard is invoked. No real package, broker, native
display, subprocess, socket, lifecycle, tool discovery or inherited environment
is used. Relay declarations are split from its channel/EOF loops with AST.
The explicit channel-tail regression runs that tail with inert stream, OS,
clock and subprocess objects; the other relay tests run declarations only.
"""

import ast
from contextlib import ExitStack, nullcontext, redirect_stderr
import hashlib
import importlib.machinery
import importlib.util
import io
import json
import os
from pathlib import Path
import signal
import stat
import struct
import subprocess
import sys
import tempfile
import threading
from types import SimpleNamespace
import unittest
from unittest import mock


REPOSITORY = Path(__file__).resolve().parents[1]
OWNER = "0" * 32  # Synthetic request ownership, never User admission.


def load_source(name, relative):
    loader = importlib.machinery.SourceFileLoader(name, str(REPOSITORY / relative))
    spec = importlib.util.spec_from_loader(name, loader)
    module = importlib.util.module_from_spec(spec)
    sys.modules[name] = module
    # Fail closed if a future top-level edit tries to launch or connect while
    # loading definitions. The real guard/entry points are never invoked.
    with ExitStack() as effects:
        for target in ("subprocess.Popen", "subprocess.run", "socket.socket", "socket.socketpair",
                       "os.system", "os.kill", "os.killpg", "os.pidfd_open", "os.waitid", "os.waitpid"):
            effects.enter_context(mock.patch(target, side_effect=AssertionError("module-load effect")))
        loader.exec_module(module)
    return module


broker = load_source("_prebuilt_contract_broker", "scripts/native-fixture-broker")
client = load_source("_prebuilt_contract_client", "scripts/native-fixture")
prebuilt = load_source("_prebuilt_contract_package", "scripts/native_fixture_prebuilt.py")


class MockOnly(unittest.TestCase):
    def setUp(self):
        stack = ExitStack()
        self.addCleanup(stack.close)
        forbidden = AssertionError("effectful seam must be explicitly mocked")
        for name in ("Popen", "run", "call", "check_call", "check_output"):
            stack.enter_context(mock.patch.object(subprocess, name, side_effect=forbidden))
        for name in ("socket", "socketpair", "create_connection"):
            stack.enter_context(mock.patch.object(prebuilt.socket, name, side_effect=forbidden))
        for name in ("system", "kill", "killpg", "pidfd_open", "waitid", "waitpid"):
            stack.enter_context(mock.patch.object(os, name, side_effect=forbidden))
        stack.enter_context(mock.patch.object(signal, "pidfd_send_signal", side_effect=forbidden))
        stack.enter_context(mock.patch.object(prebuilt.select, "select", side_effect=forbidden))
        for name in ("guard", "host_from_environment", "missing_requirements", "install_termination", "Spawner"):
            stack.enter_context(mock.patch.object(broker, name, side_effect=forbidden))


class FixedRequests(MockOnly):
    def test_client_emits_only_named_start_and_owned_fixed_step(self):
        self.assertEqual(client.request_from(client.parse(["start-prebuilt", "case-a"])),
                         {"op": "start-prebuilt", "fixture": "case-a"})
        for step in prebuilt.STEPS:
            with self.subTest(step=step):
                self.assertEqual(client.request_from(client.parse([
                    "prebuilt-step", "case-a", "--owner", OWNER, step])),
                    {"op": "prebuilt-step", "fixture": "case-a", "owner": OWNER, "step": step})
        for option in ("--checkout", "--env", "--args", "--package", "--socket"):
            with self.subTest(option=option), redirect_stderr(io.StringIO()), self.assertRaises(SystemExit):
                client.parse(["start-prebuilt", "case-a", option, "/synthetic"])

    def test_broker_rejects_path_environment_and_arbitrary_cli_fields(self):
        start = {"op": "start-prebuilt", "fixture": "case-a"}
        step = {"op": "prebuilt-step", "fixture": "case-a", "owner": OWNER, "step": "checkpoint"}
        self.assertEqual(broker.validate_request(start, Path("/synthetic")), {**start, "width": 1600})
        self.assertEqual(broker.validate_request(step, Path("/synthetic")), step)
        for request in (start, step):
            for key in ("checkout", "env", "cwd", "path", "package", "executable", "args", "socket"):
                with self.subTest(op=request["op"], key=key), self.assertRaises(broker.Refusal):
                    broker.validate_request({**request, key: "synthetic"}, Path("/synthetic"))
        for change in ({"width": 900}, {"width": True}):
            with self.subTest(change=change), self.assertRaises(broker.Refusal):
                broker.validate_request({**start, **change}, Path("/synthetic"))
        for change in ({"step": "run"}, {"step": ["checkpoint"]}, {"owner": "not-an-owner"}):
            with self.subTest(change=change), self.assertRaises(broker.Refusal):
                broker.validate_request({**step, **change}, Path("/synthetic"))

    def test_original_user_guard_precedes_argument_parser_and_prebuilt_import(self):
        # A source-order oracle, not an invocation or synthetic User guard.
        module = ast.parse((REPOSITORY / "scripts/native-fixture-broker").read_text())
        main = next(n for n in module.body if isinstance(n, ast.FunctionDef) and n.name == "main")
        first, refusal, *rest = main.body
        self.assertIsInstance(first, ast.Assign)
        self.assertEqual(ast.unparse(first.value), "guard()")
        self.assertEqual(ast.unparse(refusal.test), "refused is not None")
        self.assertEqual(ast.unparse(refusal.body[0]), "return refused")
        branch = next(n for n in rest if isinstance(n, ast.If)
                      and ast.unparse(n.test) == "arguments.prebuilt_a48")
        self.assertTrue(any(isinstance(n, ast.Return) and ast.unparse(n.value) ==
                            "module.main(sys.modules[__name__])" for n in branch.body))
        self.assertLess(refusal.end_lineno, branch.lineno)
        support = ast.parse((REPOSITORY / "scripts/native_fixture_prebuilt.py").read_text())
        support_main = next(n for n in support.body if isinstance(n, ast.FunctionDef) and n.name == "main")
        self.assertEqual(ast.unparse(support_main.body[0].value), "base.guard()")
        self.assertEqual(ast.unparse(support_main.body[1].body[0]), "return refused")
        broker.guard.assert_not_called()


class TinyPackage(MockOnly):
    def setUp(self):
        super().setUp()
        temporary = tempfile.TemporaryDirectory(prefix="prebuilt-package-contract-", dir="/tmp")
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.package = self.root / "package"
        self.package.mkdir(mode=0o700)
        self.content = {"cagents": b"synthetic cli", "cagentsd": b"synthetic daemon",
                        "cagents-ui": b"synthetic ui", ".cagents-ui-lease-v1": b""}
        self.pins = {}
        for name, content in self.content.items():
            path = self.package / name
            path.write_bytes(content)
            mode = 0o644 if name.startswith(".") else 0o755
            path.chmod(mode)
            self.pins[name] = (hashlib.sha256(content).hexdigest(), len(content), mode)
        self.opened = []

        def hold(_root, _parts):
            fd = broker.hold_directory(self.root, ("package",))
            self.opened.append(fd)
            return fd

        self.base = SimpleNamespace(Refusal=broker.Refusal, hold_directory=hold)
        stack = ExitStack()
        self.addCleanup(stack.close)
        stack.enter_context(mock.patch.object(prebuilt, "PACKAGE", self.package))
        stack.enter_context(mock.patch.object(prebuilt, "PINS", self.pins))

    def test_valid_synthetic_companions_and_closed_package_refusal(self):
        package = prebuilt.Package(self.base)
        package.verify()
        package.close()
        self.assertIsNone(package.fd)
        package.close()
        with self.assertRaises(broker.Refusal):
            package.verify()
        with self.assertRaises(OSError):
            os.fstat(self.opened[-1])

    def test_digest_type_symlink_permissions_and_link_count_are_refused(self):
        path = self.package / "cagents"
        mutations = {
            "digest": lambda: path.write_bytes(b"X" * len(self.content["cagents"])),
            "type": lambda: (path.unlink(), path.mkdir(mode=0o755)),
            "symlink": lambda: (path.unlink(), path.symlink_to("cagentsd")),
            "permissions": lambda: path.chmod(0o775),
            "hardlink": lambda: os.link(path, self.package / "second-link"),
            "directory_permissions": lambda: self.package.chmod(0o777),
        }
        for name, mutate in mutations.items():
            with self.subTest(mutation=name):
                mutate()
                with self.assertRaises((broker.Refusal, OSError)):
                    prebuilt.Package(self.base)
                with self.assertRaises(OSError):
                    os.fstat(self.opened[-1])
                if path.is_dir() and not path.is_symlink():
                    path.rmdir()
                elif path.is_symlink():
                    path.unlink()
                (self.package / "second-link").unlink(missing_ok=True)
                path.write_bytes(self.content["cagents"])
                path.chmod(0o755)
                self.package.chmod(0o700)

    def test_missing_companion_closes_original_directory_without_adoption(self):
        (self.package / "cagentsd").unlink()
        with self.assertRaises(FileNotFoundError):
            prebuilt.Package(self.base)
        with self.assertRaises(OSError):
            os.fstat(self.opened[-1])

    def test_initial_metadata_failure_closes_already_acquired_directory(self):
        original_close = os.close
        with mock.patch.object(prebuilt.os, "fstat", side_effect=OSError("synthetic initial fstat")), \
                mock.patch.object(prebuilt.os, "close", wraps=original_close) as close:
            with self.assertRaisesRegex(OSError, "synthetic initial fstat"):
                prebuilt.Package(self.base)
            target = self.opened[-1]
            target_calls = [call for call in close.call_args_list if call == mock.call(target)]
            if not target_calls:
                # Retire this known private FD immediately, before reporting a
                # regression; never retry a close with unknown disposition.
                original_close(target)
            self.assertEqual(target_calls, [mock.call(target)])


class FakePath:
    def __init__(self, value, metadata=None):
        self.value = str(value)
        self.metadata = metadata or {}

    def __str__(self):
        return self.value

    def __truediv__(self, value):
        return FakePath(self.value + "/" + str(value), self.metadata)

    def exists(self):
        return False

    def lstat(self):
        return self.metadata[self.value]


class FakeChild:
    def __init__(self, code=None):
        self.code = code
        self.pid = 1001
        self.waits = []
        self.kill = mock.Mock(side_effect=AssertionError("no kill"))
        self.terminate = mock.Mock(side_effect=AssertionError("no terminate"))
        self.send_signal = mock.Mock(side_effect=AssertionError("no signal"))

    def poll(self):
        return self.code

    def wait(self, timeout=None):
        self.waits.append(timeout)
        if self.code is None:
            raise subprocess.TimeoutExpired("synthetic child", timeout)
        return self.code


class RelayHarness:
    def __init__(self):
        self.streams = []
        self.launched = []
        self.next_child = FakeChild()
        descriptors = iter(range(1000, 1100))  # Numbers never reach the real OS.
        def fdopen(_fd, _mode):
            stream = mock.Mock(name="synthetic capture", closed=False)
            self.streams.append(stream)
            return stream
        def popen(argv, **kwargs):
            self.launched.append((argv, kwargs, self.next_child))
            return self.next_child
        self.os = SimpleNamespace(open=mock.Mock(side_effect=lambda *_a, **_k: next(descriptors)),
                                  fdopen=mock.Mock(side_effect=fdopen), close=mock.Mock(),
                                  listdir=mock.Mock(return_value=["100"]), getpid=lambda: 100,
                                  geteuid=lambda: 1000, path=SimpleNamespace(lexists=mock.Mock(return_value=False)),
                                  waitid=mock.Mock(return_value=None),
                                  waitpid=mock.Mock(side_effect=AssertionError("no admitted synthetic exited child")),
                                  P_ALL=os.P_ALL, WEXITED=os.WEXITED, WNOHANG=os.WNOHANG, WNOWAIT=os.WNOWAIT,
                                  O_WRONLY=os.O_WRONLY, O_CREAT=os.O_CREAT, O_EXCL=os.O_EXCL)
        self.processes = SimpleNamespace(Popen=mock.Mock(side_effect=popen), DEVNULL=-3,
                                         TimeoutExpired=subprocess.TimeoutExpired)
        self.socket = SimpleNamespace(socket=mock.Mock(side_effect=AssertionError("channel was executed")),
                                      AF_UNIX=prebuilt.socket.AF_UNIX, SOL_SOCKET=prebuilt.socket.SOL_SOCKET,
                                      SO_PEERCRED=prebuilt.socket.SO_PEERCRED)
        self.metadata = {
            "/run/cagents-fixture/cagents": SimpleNamespace(st_mode=stat.S_IFDIR | 0o700, st_uid=1000),
            "/run/cagents-fixture/cagents/cagentsd.sock": SimpleNamespace(st_mode=stat.S_IFSOCK | 0o600, st_uid=1000),
            "/run/cagents-fixture/cagents/cagentsd.lock": SimpleNamespace(st_mode=stat.S_IFREG | 0o600, st_uid=1000, st_nlink=1),
        }
        tree = ast.parse(prebuilt.RELAY)
        boundary = next(i for i, node in enumerate(tree.body) if isinstance(node, ast.Assign)
                        and any(isinstance(t, ast.Name) and t.id == "channel" for t in node.targets))
        prefix = [n for n in tree.body[:boundary] if not isinstance(n, (ast.Import, ast.ImportFrom))]
        self.namespace = {"json": __import__("json"), "os": self.os,
                          "pathlib": SimpleNamespace(Path=lambda value: FakePath(value, self.metadata)), "socket": self.socket,
                          "subprocess": self.processes, "sys": SimpleNamespace(argv=["relay", "3"]),
                          "stat": stat, "struct": struct,
                          "time": SimpleNamespace(monotonic=mock.Mock(return_value=0), sleep=mock.Mock())}
        exec(compile(ast.Module(body=prefix, type_ignores=[]), "<inert prebuilt RELAY prefix>", "exec"), self.namespace)
        self.socket.socket.assert_not_called()
        self.owned_endpoint = self.namespace["owned_endpoint"]
        # Ordering tests isolate readiness; its actual proof predicate has a
        # separate all-mocked test below, never a real endpoint connection.
        self.namespace["owned_endpoint"] = mock.Mock(return_value=True)

    def run(self, action):
        return self.namespace["run"]({"action": action})

    def finish(self, role, code=0):
        self.namespace["children"][role]["child"].code = code

    def ready(self):
        self.next_child = FakeChild(0)
        result = self.run("ready")
        if result.get("exit_code") != 0 or result.get("settled") is not True:
            raise AssertionError("synthetic original ready child must settle successfully")
        self.next_child = FakeChild()

    def no_signals(self):
        for _argv, _kwargs, child in self.launched:
            child.kill.assert_not_called()
            child.terminate.assert_not_called()
            child.send_signal.assert_not_called()


class NamedLifecycle(MockOnly):
    def test_adopted_exit_receipt_precedes_one_exact_wait_and_reap_is_bounded(self):
        relay = RelayHarness()
        peek = SimpleNamespace(si_pid=200, si_uid=1000, si_code=os.CLD_EXITED, si_status=0)
        relay.os.waitid.side_effect = [peek, None]
        def consume(pid, flags):
            receipt = relay.namespace["adopted_exits"][0]
            self.assertEqual(receipt, {"pid": 200, "uid": 1000, "code": os.CLD_EXITED,
                                       "status": 0, "settled": False, "waitpid": None, "error": None})
            self.assertEqual((pid, flags), (200, os.WNOHANG))
            return pid, 0
        relay.os.waitpid.side_effect = consume
        self.assertTrue(relay.namespace["reap_exited"]())
        relay.os.waitpid.assert_called_once_with(200, os.WNOHANG)
        relay.os.waitid.assert_called_with(os.P_ALL, 0, os.WEXITED | os.WNOHANG | os.WNOWAIT)
        relay.os.waitid.side_effect = None
        self.assertFalse(relay.run("status")["wait_hold"])
        relay.os.waitpid.assert_called_once_with(200, os.WNOHANG)
        capped = RelayHarness()
        capped.os.waitid.side_effect = [SimpleNamespace(si_pid=pid, si_uid=1000,
                                                       si_code=os.CLD_EXITED, si_status=0)
                                      for pid in range(200, 328)]
        capped.os.waitpid.side_effect = lambda pid, _flags: (pid, 0)
        self.assertFalse(capped.namespace["reap_exited"]())
        self.assertEqual(capped.os.waitid.call_count, 128)
        timed = RelayHarness()
        timed.namespace["time"].monotonic.side_effect = [0, .5]
        self.assertFalse(timed.namespace["reap_exited"]())
        timed.os.waitid.assert_not_called()

    def test_original_popen_wait_precedes_generic_reaping_and_keeps_its_owner(self):
        relay = RelayHarness()
        relay.next_child = FakeChild(0)
        original = relay.namespace["spawn"]("daemon", ["/synthetic/never-executed"])
        def peek_after_original(*_args):
            self.assertTrue(original["settled"])
            self.assertEqual(original["child"].waits, [None])
            return None
        relay.os.waitid.side_effect = peek_after_original
        result = relay.run("status")
        self.assertTrue(next(iter(result["children"].values()))["original_wait"])
        relay.os.waitpid.assert_not_called()
        self.assertEqual(relay.namespace["adopted_exits"], [])
        self.assertIs(relay.namespace["history"][0], original)
        for stream in original["streams"]:
            stream.close.assert_called_once_with()

    def test_historic_settled_pid_does_not_hide_new_adopted_exit_receipt(self):
        relay = RelayHarness()
        relay.next_child = FakeChild(0)
        original = relay.namespace["spawn"]("daemon", ["/synthetic/never-executed"])
        relay.namespace["states"]()
        pid = original["child"].pid
        relay.os.waitid.side_effect = [SimpleNamespace(si_pid=pid, si_uid=1000,
                                                       si_code=os.CLD_EXITED, si_status=3), None]
        relay.os.waitpid.side_effect = lambda observed, _flags: (observed, 3 << 8)
        result = relay.run("shutdown")
        self.assertTrue(result["settled"])
        relay.os.waitpid.assert_called_once_with(pid, os.WNOHANG)
        self.assertEqual(original["child"].waits, [None])
        self.assertEqual(result["adopted_waits"], [{"pid": pid, "uid": 1000, "code": os.CLD_EXITED,
                                                  "status": 3, "settled": True,
                                                  "waitpid": [pid, 3 << 8], "error": None}])

    def test_zero_wrong_pid_or_echild_wait_stays_held_without_retry(self):
        for label, outcome in (("zero", (0, 0)), ("wrong-pid", (999, 0)),
                               ("ECHILD", ChildProcessError("synthetic ECHILD"))):
            with self.subTest(outcome=label):
                relay = RelayHarness()
                relay.os.waitid.return_value = SimpleNamespace(si_pid=200, si_uid=1000,
                                                               si_code=os.CLD_EXITED, si_status=0)
                relay.os.waitpid.side_effect = outcome if isinstance(outcome, BaseException) else None
                if not isinstance(outcome, BaseException):
                    relay.os.waitpid.return_value = outcome
                with self.assertRaisesRegex(ValueError, "adopted-child wait"):
                    relay.run("shutdown")
                receipt = relay.namespace["adopted_exits"][0]
                self.assertFalse(receipt["settled"])
                self.assertEqual(receipt["pid"], 200)
                if label == "ECHILD":
                    self.assertEqual(receipt["error"], "ChildProcessError")
                else:
                    self.assertEqual(receipt["waitpid"], list(outcome))
                with self.assertRaisesRegex(ValueError, "adopted-child wait"):
                    relay.run("daemon-start")
                self.assertTrue(relay.run("status")["wait_hold"])
                self.assertFalse(relay.namespace["closing"])
                relay.os.waitid.assert_called_once_with(os.P_ALL, 0, os.WEXITED | os.WNOHANG | os.WNOWAIT)
                relay.os.waitpid.assert_called_once_with(200, os.WNOHANG)
                relay.processes.Popen.assert_not_called()

    def test_owned_endpoint_requires_original_peer_and_private_socket_lock(self):
        relay = RelayHarness()
        child = FakeChild()
        relay.namespace["children"]["daemon"] = {"child": child}
        probe = mock.MagicMock()
        probe.__enter__.return_value = probe
        relay.socket.socket.side_effect = None
        relay.socket.socket.return_value = probe
        for pid, uid, accepted in ((1001, 1000, True), (1002, 1000, False), (1001, 1002, False)):
            with self.subTest(pid=pid, uid=uid):
                probe.getsockopt.return_value = struct.pack("3i", pid, uid, 1000)
                self.assertEqual(relay.owned_endpoint(), accepted)
        probe.getsockopt.return_value = struct.pack("3i", 1001, 1000, 1000)
        for path, field, invalid in (("cagentsd.sock", "st_mode", stat.S_IFREG | 0o600),
                                     ("cagentsd.sock", "st_mode", stat.S_IFSOCK | 0o666),
                                     ("cagentsd.lock", "st_nlink", 2)):
            record = relay.metadata["/run/cagents-fixture/cagents/" + path]
            old = getattr(record, field)
            with self.subTest(path=path, field=field):
                setattr(record, field, invalid)
                self.assertFalse(relay.owned_endpoint())
                setattr(record, field, old)
        child.code = 0
        before = relay.socket.socket.call_count
        self.assertFalse(relay.owned_endpoint())
        self.assertEqual(relay.socket.socket.call_count, before)

    def test_completed_generations_remain_in_original_history(self):
        relay = RelayHarness()
        relay.run("daemon-start")
        first = relay.namespace["children"]["daemon"]
        relay.finish("daemon")
        relay.next_child = FakeChild()
        relay.run("daemon-start")
        self.assertEqual(len(relay.namespace["history"]), 2)
        self.assertIs(relay.namespace["history"][0], first)
        self.assertIsNot(relay.namespace["children"]["daemon"], first)
        self.assertTrue(first["settled"])
        self.assertEqual([row["original_wait"] for row in relay.run("status")["children"].values()], [True, False])
        for stream in first["streams"]:
            stream.close.assert_called_once_with()

    def test_successful_ready_receipt_cannot_move_to_a_new_daemon_generation(self):
        relay = RelayHarness()
        relay.run("daemon-start")
        relay.ready()
        first = relay.namespace["children"]["daemon"]
        relay.finish("daemon")
        relay.next_child = FakeChild()
        relay.run("daemon-start")
        with self.assertRaisesRegex(ValueError, "readiness receipt"):
            relay.run("ui-start")
        self.assertIsNot(relay.namespace["children"]["daemon"], first)
        relay.ready()
        self.assertEqual(relay.run("ui-start")["started"], "ui-start")
        relay.no_signals()

    def test_live_daemon_and_ui_cannot_be_replaced(self):
        relay = RelayHarness()
        self.assertEqual(relay.run("daemon-start")["started"], "daemon-start")
        with self.assertRaisesRegex(ValueError, "original daemon"):
            relay.run("daemon-start")
        with self.assertRaisesRegex(ValueError, "readiness receipt"):
            relay.run("ui-start")
        relay.ready()
        relay.next_child = FakeChild()
        relay.run("ui-start")
        with self.assertRaisesRegex(ValueError, "UI/window"):
            relay.run("ui-start")
        self.assertEqual(len(relay.launched), 3)
        relay.no_signals()

    def test_ui_original_wait_precedes_checkpoint_and_fixed_cli_is_private(self):
        relay = RelayHarness()
        relay.run("daemon-start")
        relay.ready()
        relay.next_child = FakeChild()
        relay.run("ui-start")
        with self.assertRaisesRegex(ValueError, "WINDOW"):
            relay.run("checkpoint")
        self.assertFalse(relay.run("ui-wait")["settled"])
        self.assertEqual(len(relay.launched), 3)
        relay.finish("ui")
        self.assertTrue(relay.run("ui-wait")["settled"])
        relay.next_child = FakeChild(0)
        result = relay.run("checkpoint")
        self.assertEqual(result, {"settled": True, "exit_code": 0, "failed": False})
        argv, kwargs, _child = relay.launched[-1]
        self.assertEqual(argv, ["/opt/cagents-a48/cagents", "--device", "local", "--no-start", "--json",
                                "daemon", "prepare-upgrade", "--yes", "--no-relaunch-agents"])
        self.assertEqual(kwargs["env"], prebuilt.environment())
        self.assertEqual(kwargs["cwd"], "/fixture/project")
        self.assertNotIn("stdout", result)
        self.assertNotIn("stderr", result)
        relay.no_signals()

    def test_control_timeout_retains_original_and_blocks_new_work(self):
        relay = RelayHarness()
        relay.run("daemon-start")
        relay.next_child = FakeChild()
        self.assertEqual(relay.run("preflight"), {"settled": False, "failed": True})
        controls = {k: v for k, v in relay.namespace["children"].items() if k.startswith("control")}
        self.assertEqual(len(controls), 1)
        record = next(iter(controls.values()))
        self.assertFalse(record["settled"])
        self.assertEqual(len(record["streams"]), 2)
        for stream in record["streams"]:
            stream.close.assert_not_called()
        for action in ("checkpoint", "empty-stop", "shutdown"):
            with self.subTest(action=action), self.assertRaisesRegex(ValueError, "control child"):
                relay.run(action)
        self.assertEqual(len(relay.launched), 2)
        relay.no_signals()

    def test_negative_exit_allows_cleanup_wait_only_and_shutdown_requires_no_unknown(self):
        relay = RelayHarness()
        relay.run("daemon-start")
        relay.next_child = FakeChild(7)
        self.assertEqual(relay.run("preflight")["exit_code"], 7)
        with self.assertRaisesRegex(ValueError, "negative retained"):
            relay.run("ui-start")
        relay.next_child = FakeChild(0)
        self.assertEqual(relay.run("empty-stop")["exit_code"], 0)
        relay.finish("daemon")
        self.assertTrue(relay.run("daemon-wait")["settled"])
        relay.os.listdir.return_value = ["100", "101", "self", "net"]
        with self.assertRaisesRegex(ValueError, "unknown processes"):
            relay.run("shutdown")
        self.assertFalse(relay.namespace["closing"])
        relay.os.listdir.return_value = ["100", "self"]
        self.assertTrue(relay.run("shutdown")["settled"])
        self.assertTrue(relay.namespace["closing"])
        relay.no_signals()

    def test_capture_close_uncertainty_retains_record_and_refuses_shutdown(self):
        relay = RelayHarness()
        relay.run("daemon-start")
        relay.finish("daemon")
        relay.streams[0].close.side_effect = OSError("synthetic uncertain close")
        with self.assertRaisesRegex(ValueError, "capture close"):
            relay.run("shutdown")
        self.assertIn("daemon", relay.namespace["children"])
        self.assertTrue(relay.namespace["children"]["daemon"]["close_unknown"])
        self.assertFalse(relay.namespace["closing"])
        relay.no_signals()

    def test_second_capture_failure_closes_first_and_never_spawns(self):
        relay = RelayHarness()
        relay.os.open.side_effect = [1000, OSError("synthetic second capture")]
        with self.assertRaisesRegex(OSError, "second capture"):
            relay.run("daemon-start")
        relay.streams[0].close.assert_called_once_with()
        relay.processes.Popen.assert_not_called()

    def test_fdopen_failure_retires_new_raw_capture_before_losing_record(self):
        relay = RelayHarness()
        relay.os.fdopen.side_effect = OSError("synthetic fdopen")
        with self.assertRaisesRegex(OSError, "synthetic fdopen"):
            relay.run("daemon-start")
        relay.os.close.assert_called_once_with(1000)
        relay.processes.Popen.assert_not_called()
        uncertain = RelayHarness()
        uncertain.os.fdopen.side_effect = OSError("synthetic fdopen")
        uncertain.os.close.side_effect = OSError("synthetic unknown close")
        with self.assertRaisesRegex(OSError, "synthetic fdopen"):
            uncertain.run("daemon-start")
        uncertain.os.close.assert_called_once_with(1000)
        self.assertIn("daemon", uncertain.namespace["children"])
        self.assertTrue(uncertain.namespace["history"][0]["close_unknown"])
        with self.assertRaisesRegex(ValueError, "capture close"):
            uncertain.run("shutdown")
        uncertain.os.close.assert_called_once_with(1000)


class PrivateBoundaryAndCleanup(MockOnly):
    def fixtures(self):
        kind = prebuilt.install(broker)
        fixtures = object.__new__(kind)  # No Fixtures constructor, thread or spawner.
        fixtures.channel_lock = threading.RLock()
        return fixtures

    def test_environment_and_mounts_do_not_forward_host_home_build_desktop_or_tokens(self):
        expected = {"HOME": "/fixture/home", "USER": "fixture", "LOGNAME": "fixture",
                    "PATH": "/fixture/bin:/usr/bin:/bin", "LANG": "C.UTF-8", "TERM": "dumb",
                    "TMPDIR": "/tmp", "XDG_RUNTIME_DIR": "/run/cagents-fixture",
                    "XDG_DATA_HOME": "/fixture/data", "XDG_CONFIG_HOME": "/fixture/config",
                    "XDG_CACHE_HOME": "/fixture/cache", "XDG_STATE_HOME": "/fixture/state",
                    "XDG_SESSION_TYPE": "wayland", "WAYLAND_DISPLAY": "nested-wayland"}
        self.assertEqual(prebuilt.environment(), expected)
        fixture = SimpleNamespace(layout=SimpleNamespace())
        binds = SimpleNamespace(checkout=11, target=12, sandbox=13, runtime=14, wayland=15)
        with mock.patch.object(Path, "is_symlink", return_value=False), \
                mock.patch.object(Path, "is_dir", return_value=False):
            argv = self.fixtures().holder_args(fixture, binds, 16, 17)
        split = argv.index("--")
        mount_args = argv[:split]
        self.assertEqual(argv[split + 1:], ["/usr/bin/python3", "-I", "-c", prebuilt.RELAY, "17"])
        self.assertIn("--unshare-all", mount_args)
        self.assertIn("--as-pid-1", mount_args)
        self.assertEqual(mount_args[mount_args.index("--ro-bind-fd") + 1:][:2], ["11", "/opt/cagents-a48"])
        self.assertNotIn("--share-net", mount_args)
        for forbidden in ("/home", "/run/user", "/root", "cargo", "CAGENTS_AGENT_AUTHORITY", "DBUS_SESSION_BUS_ADDRESS"):
            self.assertFalse(any(forbidden in item for item in mount_args), forbidden)
        for source, destination in (("13", "/fixture"), ("14", "/run/cagents-fixture"),
                                    ("15", "/run/cagents-fixture/nested-wayland")):
            self.assertTrue(any(mount_args[i:i + 3] == ["--bind-fd", source, destination]
                                for i in range(len(mount_args))))

    def test_fixed_mode_rejects_build_arbitrary_cli_and_stale_adoption(self):
        fixtures = self.fixtures()
        for op in ("start", "clear", "run-ui", "cli"):
            with self.subTest(op=op), self.assertRaisesRegex(broker.Refusal, "fixed prebuilt mode"):
                fixtures.handle({"op": op})
        self.assertEqual(fixtures.recover_stale(), ([], []))

    def test_channel_or_receipt_failure_keeps_original_fixture_and_roots(self):
        fixtures = self.fixtures()
        process = FakeChild()
        fixture = SimpleNamespace(layout=SimpleNamespace(name="case-a"), process=process,
                                  channel=SimpleNamespace(call=mock.Mock(side_effect=TimeoutError("original timeout"))),
                                  pidfd=19, released=False, stopped=False)
        fixtures.live = {"case-a": fixture}
        with mock.patch.object(fixtures, "_discard") as discard:
            with self.assertRaisesRegex(broker.Refusal, "original timeout"):
                fixtures.stop(fixture, {"keep": False})
            discard.assert_not_called()
        self.assertIs(fixtures.live["case-a"], fixture)
        self.assertFalse(fixture.settlement["settled"])
        self.assertFalse(fixture.released)
        process.kill.assert_not_called()
        process.terminate.assert_not_called()
        self.assertEqual(process.waits, [])

        # A successful relay reply is not a completed private evidence write.
        # A retry must not skip a failed write and release the original holder.
        writer_child = FakeChild(0)
        writer_fixture = SimpleNamespace(layout=SimpleNamespace(name="case-b", private=Path("/synthetic/private")),
                                         process=writer_child, pidfd=19, spawn_resources={},
                                         channel=SimpleNamespace(call=mock.Mock(return_value={
                                             "settled": True, "adopted_waits": [{"pid": 200, "settled": True}]})),
                                         released=False, stopped=False)
        fixtures.live = {"case-b": writer_fixture}
        with mock.patch.object(prebuilt.os, "open", return_value=2000) as opened, \
                mock.patch.object(prebuilt.os, "write", side_effect=OSError("synthetic receipt write")) as written, \
                mock.patch.object(prebuilt.select, "select", return_value=([19], [], [])), \
                mock.patch.object(fixtures, "_close_spawn_resources", return_value=[]), \
                mock.patch.object(fixtures, "_discard") as discard:
            for _attempt in range(2):
                with self.assertRaises(broker.Refusal):
                    fixtures.stop(writer_fixture, {"keep": False})
                self.assertFalse(writer_fixture.settlement["settled"])
            opened.assert_called_once()
            written.assert_called_once()
            discard.assert_not_called()
        self.assertIs(fixtures.live["case-b"], writer_fixture)
        self.assertEqual(writer_child.waits, [])
        writer_child.kill.assert_not_called()
        writer_child.terminate.assert_not_called()

    def test_receipt_precedes_original_wait_and_pidfd_exit_is_still_required(self):
        fixtures = self.fixtures()
        for code, ready in ((None, [19]), (0, [])):
            with self.subTest(code=code, ready=ready):
                child = FakeChild(code)
                events = []
                original_wait = child.wait
                def wait(timeout=None):
                    events.append("original-wait")
                    return original_wait(timeout=timeout)
                child.wait = wait
                fixture = SimpleNamespace(process=child, pidfd=19, spawn_resources={},
                                          layout=SimpleNamespace(private=Path("/synthetic/private")),
                                          channel=SimpleNamespace(call=mock.Mock(return_value={
                                              "settled": True, "adopted_waits": []})))
                def opened(*_args, **_kwargs):
                    events.append("receipt-open")
                    return 2000
                def written(_fd, payload):
                    events.append("receipt-write")
                    self.assertEqual(__import__("json").loads(payload), {"adopted_waits": []})
                    return len(payload)
                def close_resources(*_args):
                    events.append("receipt-close")
                    fixture.spawn_resources.clear()
                    return []
                with mock.patch.object(prebuilt.os, "open", side_effect=opened), \
                        mock.patch.object(prebuilt.os, "write", side_effect=written), \
                        mock.patch.object(fixtures, "_close_spawn_resources", side_effect=close_resources), \
                        mock.patch.object(prebuilt.select, "select", return_value=(ready, [], [])):
                    result = fixtures._kill_holder(fixture)
                self.assertFalse(result["settled"])
                self.assertEqual(events, ["receipt-open", "receipt-write", "receipt-close", "original-wait"])
                fixture.channel.call.assert_called_once_with({"action": "shutdown"}, 10)
                child.kill.assert_not_called()
                child.terminate.assert_not_called()

    def test_desktop_cleanup_uncertainty_prevents_root_discard(self):
        fixtures = self.fixtures()
        layout = SimpleNamespace(name="case-a")
        fixture = SimpleNamespace(layout=layout, desktop=SimpleNamespace(
            stop=mock.Mock(side_effect=broker.Refusal("cleanup_incomplete", "original display wait unknown"))),
            host_session_stopped=False)
        fixtures.live = {"case-a": fixture}
        with mock.patch.object(broker.Fixtures, "_discard") as discard:
            with self.assertRaisesRegex(broker.Refusal, "original display wait unknown"):
                fixtures._discard(layout, remove=True)
            discard.assert_not_called()
        self.assertFalse(fixture.host_session_stopped)
        fixtures.live.clear()
        with self.assertRaisesRegex(broker.Refusal, "stale root is retained"):
            fixtures._discard(layout, remove=True)

    def test_desktop_without_quit_endpoint_never_signals_original_siblings(self):
        desktop = prebuilt.Desktop(broker, SimpleNamespace(private=Path("/synthetic/private"),
                                                         niri_root=Path("/synthetic/niri")))
        for role in ("niri", "kwin", "bus"):
            record = {"role": role, "child": FakeChild(), "streams": [], "waited": False,
                      "close_unknown": False}
            desktop.records.append(record)
            desktop.children[role] = record
        with self.assertRaisesRegex(broker.Refusal, "without a usable Quit endpoint"):
            desktop.stop()
        for record in desktop.records:
            record["child"].kill.assert_not_called()
            record["child"].terminate.assert_not_called()
            self.assertFalse(record["waited"])

    def test_desktop_fdopen_failure_keeps_or_closes_original_raw_capture(self):
        desktop = prebuilt.Desktop(broker, SimpleNamespace(private=Path("/synthetic/private"),
                                                         niri_root=Path("/synthetic/niri")))
        with mock.patch.object(prebuilt.os, "open", return_value=2000), \
                mock.patch.object(prebuilt.os, "fdopen", side_effect=OSError("synthetic desktop fdopen")), \
                mock.patch.object(prebuilt.os, "close") as close:
            with self.assertRaisesRegex(OSError, "synthetic desktop fdopen"):
                desktop.spawn("bus", ["/synthetic/never-executed"])
            close.assert_called_once_with(2000)
        subprocess.Popen.assert_not_called()


class QuitAndChannel(MockOnly):
    def desktop(self):
        desktop = prebuilt.Desktop(broker, SimpleNamespace(private=Path("/synthetic/private"),
                                                         niri_root=Path("/synthetic/niri")))
        desktop.ipc_fd = 37  # Synthetic number, never passed to the real OS.
        desktop.ipc = "/synthetic/held-original-niri-socket"
        for role in ("niri", "kwin", "bus"):
            record = {"role": role, "child": FakeChild(), "streams": [], "waited": False,
                      "close_unknown": False, "closed_streams": set(), "raw_fds": [],
                      "spawn_unknown": False}
            desktop.records.append(record)
            desktop.children[role] = record
        return desktop

    def connection(self, *, pid=1001, uid=1000, replies=()):
        connection = mock.MagicMock(name="synthetic original niri stream")
        connection.__enter__.return_value = connection
        connection.__exit__.return_value = False
        connection.getsockopt.return_value = struct.pack("3i", pid, uid, 1000)
        connection.recv.side_effect = list(replies)
        return connection

    def test_quit_uses_same_verified_original_peer_stream_and_ack_is_not_child_wait(self):
        for pid, uid in ((1002, 1000), (1001, 1002)):
            with self.subTest(pid=pid, uid=uid):
                desktop = self.desktop()
                connection = self.connection(pid=pid, uid=uid)
                with mock.patch.object(prebuilt.os, "geteuid", return_value=1000), \
                        mock.patch.object(prebuilt.socket, "socket", return_value=connection) as factory:
                    with self.assertRaisesRegex(broker.Refusal, "not the original Child"):
                        desktop.owned_connection()
                    factory.assert_called_once_with(prebuilt.socket.AF_UNIX)
                    connection.close.assert_called_once_with()
                    connection.sendall.assert_not_called()
        desktop = self.desktop()
        connection = self.connection(replies=(b'{"Ok":', b'"Handled"}\n'))
        with mock.patch.object(prebuilt.os, "geteuid", return_value=1000), \
                mock.patch.object(prebuilt.time, "monotonic", return_value=0), \
                mock.patch.object(prebuilt.socket, "socket", return_value=connection) as factory:
            desktop.quit_original()
            self.assertTrue(desktop.quit_ack)
            self.assertTrue(desktop.quit_attempted)
            factory.assert_called_once_with(prebuilt.socket.AF_UNIX)
            connection.connect.assert_called_once_with(desktop.ipc)
            connection.getsockopt.assert_called_once_with(prebuilt.socket.SOL_SOCKET, prebuilt.socket.SO_PEERCRED, 12)
            connection.sendall.assert_called_once_with(b'{"Action":{"Quit":{"skip_confirmation":true}}}\n')
            call_names = [call[0] for call in connection.mock_calls]
            self.assertLess(call_names.index("getsockopt"), call_names.index("sendall"))
            self.assertEqual(connection.recv.call_count, 2)
            desktop.quit_original()
            factory.assert_called_once_with(prebuilt.socket.AF_UNIX)
            connection.sendall.assert_called_once()
        for record in desktop.records:
            self.assertEqual(record["child"].waits, [])
            self.assertFalse(record["waited"])
            record["child"].kill.assert_not_called()
            record["child"].terminate.assert_not_called()

    def test_incomplete_oversize_malformed_or_err_quit_never_resends_or_signals_siblings(self):
        responses = {"EOF": [b""], "partial": [b'{"Ok":"Handled"}', b""],
                     "oversize": [b"x" * 1024] * 4 + [b"x"],
                     "malformed": [b"{broken}\n"], "Err": [b'{"Err":"refused"}\n']}
        for label, replies in responses.items():
            with self.subTest(reply=label):
                desktop = self.desktop()
                connection = self.connection(replies=replies)
                with mock.patch.object(prebuilt.os, "geteuid", return_value=1000), \
                        mock.patch.object(prebuilt.time, "monotonic", return_value=0), \
                        mock.patch.object(prebuilt.socket, "socket", return_value=connection) as factory:
                    with self.assertRaises(broker.Refusal):
                        desktop.quit_original()
                    self.assertTrue(desktop.quit_attempted)
                    self.assertFalse(desktop.quit_ack)
                    desktop.children["niri"]["child"].code = 0  # Later fake natural exit cannot repair unknown ACK.
                    with self.assertRaisesRegex(broker.Refusal, "acknowledgement unknown"):
                        desktop.quit_original()
                    with self.assertRaisesRegex(broker.Refusal, "acknowledgement unknown"):
                        desktop.stop()
                    factory.assert_called_once_with(prebuilt.socket.AF_UNIX)
                    connection.sendall.assert_called_once()
                    if label == "oversize":
                        connection.recv.assert_called_with(1)
                self.assertFalse(desktop.ipc_close_unknown)
                self.assertEqual(desktop.ipc_fd, 37)
                for record in desktop.records:
                    self.assertEqual(record["child"].waits, [])
                    self.assertFalse(record["waited"])
                    record["child"].kill.assert_not_called()
                    record["child"].terminate.assert_not_called()
                    record["child"].send_signal.assert_not_called()

    def test_relay_eof_or_broken_channel_retains_live_original_until_natural_wait(self):
        tree = ast.parse(prebuilt.RELAY)
        boundary = next(i for i, node in enumerate(tree.body) if isinstance(node, ast.Assign)
                        and any(isinstance(t, ast.Name) and t.id == "channel" for t in node.targets))
        tail = compile(ast.Module(body=tree.body[boundary:], type_ignores=[]), "<inert prebuilt RELAY tail>", "exec")
        for label, error in (("EOF", None), ("broken-greeting", BrokenPipeError), ("malformed-request", ValueError)):
            with self.subTest(channel=label):
                relay = RelayHarness()
                original = relay.namespace["spawn"]("daemon", ["/synthetic/never-executed"])
                child = original["child"]
                channel, stream = mock.MagicMock(), mock.MagicMock()
                stream.__enter__.return_value = stream
                stream.__exit__.return_value = False
                stream.__iter__.return_value = iter([b"{broken}\n"] if label == "malformed-request" else [])
                if label == "broken-greeting":
                    stream.write.side_effect = BrokenPipeError("synthetic greeting failure")
                channel.makefile.return_value = stream
                relay.socket.socket.side_effect = None
                relay.socket.socket.return_value = channel
                before_exit = []
                def observed_sleep(delay):
                    before_exit.append({"delay": delay, "retained": relay.namespace["children"].get("daemon") is original,
                                        "settled": original["settled"], "waits": list(child.waits),
                                        "capture_closes": [capture.close.call_count for capture in original["streams"]],
                                        "signals": child.kill.call_count + child.terminate.call_count + child.send_signal.call_count})
                    child.code = 0  # Only fake natural exit, after observing retained live custody.
                    if len(before_exit) > 1:
                        # Stop a regressed mocked loop finitely; the assertions
                        # below fail if this test-only abort was needed.
                        relay.namespace["closing"] = True
                relay.namespace["time"].sleep.side_effect = observed_sleep
                expectation = self.assertRaises(error) if error is not None else nullcontext()
                with expectation:
                    exec(tail, relay.namespace)
                self.assertEqual(before_exit, [{"delay": .1, "retained": True, "settled": False,
                                                "waits": [], "capture_closes": [0, 0], "signals": 0}])
                self.assertFalse(relay.namespace["closing"])
                self.assertIs(relay.namespace["history"][0], original)
                self.assertTrue(original["settled"])
                self.assertEqual(child.waits, [None])
                for capture in original["streams"]:
                    capture.close.assert_called_once_with()
                relay.socket.socket.assert_called_once_with(fileno=3)
                channel.makefile.assert_called_once_with("rwb")
                relay.os.waitid.assert_called_with(os.P_ALL, 0, os.WEXITED | os.WNOHANG | os.WNOWAIT)
                relay.os.waitpid.assert_not_called()
                relay.no_signals()


class PublicationAndChannel(MockOnly):
    def test_handshake_publishes_only_closed_complete_record_without_overwrite(self):
        pending = Path("/synthetic/handshake.pending")
        final = Path("/synthetic/handshake.json")
        expected = {"display": "wayland-synthetic", "socket": "/synthetic/niri.sock"}
        for fault in (None, "write", "close", "existing-final"):
            with self.subTest(fault=fault):
                files = {final: "original final"} if fault == "existing-final" else {}
                events = []
                retained = {}

                class Capture(io.StringIO):
                    def write(self, text):
                        if fault != "existing-final":
                            self_test.assertNotIn(final, files)
                        if fault == "write":
                            raise OSError("synthetic pending write")
                        return super().write(text)

                    def close(self):
                        if not self.closed:
                            retained["text"] = self.getvalue()
                            events.append("closed-pending")
                            super().close()
                            if fault == "close":
                                raise OSError("synthetic pending close")

                self_test = self
                capture = Capture()
                def opened(path, flags, mode):
                    self.assertEqual((path, flags, mode), (pending, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600))
                    self.assertNotIn(pending, files)
                    files[pending] = capture
                    events.append("opened-pending")
                    return 2000
                def fdopen(fd, mode):
                    self.assertEqual((fd, mode), (2000, "w"))
                    return capture
                def link(source, destination):
                    self.assertEqual((source, destination), (pending, final))
                    self.assertTrue(capture.closed)
                    self.assertEqual(json.loads(retained["text"]), expected)
                    if destination in files:
                        raise FileExistsError("synthetic existing final")
                    files[destination] = retained["text"]
                    events.append("published-complete")
                def unlink(path):
                    self.assertEqual(path, pending)
                    self.assertEqual(json.loads(files[final]), expected)
                    del files[path]
                    events.append("removed-pending")
                fake_os = SimpleNamespace(open=mock.Mock(side_effect=opened), fdopen=mock.Mock(side_effect=fdopen),
                                          link=mock.Mock(side_effect=link), unlink=mock.Mock(side_effect=unlink),
                                          O_WRONLY=os.O_WRONLY, O_CREAT=os.O_CREAT, O_EXCL=os.O_EXCL,
                                          environ={"WAYLAND_DISPLAY": expected["display"], "NIRI_SOCKET": expected["socket"]})
                imports = {"json": json, "os": fake_os, "pathlib": SimpleNamespace(Path=Path),
                           "sys": SimpleNamespace(argv=["handshake", str(final)])}
                imported = []
                def synthetic_import(name, *_args, **_kwargs):
                    imported.append(name)
                    return imports[name]  # No fallback to real os/environment.
                namespace = {"__builtins__": {"__import__": synthetic_import}}
                expectation = nullcontext() if fault is None else self.assertRaises(OSError)
                with expectation:
                    exec(compile(prebuilt.HANDSHAKE, "<inert prebuilt handshake>", "exec"), namespace)
                self.assertEqual(set(imported), {"json", "os", "pathlib", "sys"})
                if fault is None:
                    self.assertEqual(events, ["opened-pending", "closed-pending", "published-complete", "removed-pending"])
                    self.assertNotIn(pending, files)
                    self.assertEqual(json.loads(files[final]), expected)
                else:
                    fake_os.unlink.assert_not_called()
                    self.assertIn(pending, files)
                    self.assertEqual(files.get(final), "original final" if fault == "existing-final" else None)
                    if fault in ("write", "close"):
                        fake_os.link.assert_not_called()

    def test_status_and_step_serialize_with_early_cleanup_on_original_channel(self):
        for operation in ("status", "prebuilt-step"):
            with self.subTest(operation=operation):
                fixtures = object.__new__(prebuilt.install(broker))
                fixtures.channel_lock = threading.RLock()
                fixtures.package = SimpleNamespace(verify=mock.Mock())
                first_entered, release_first, overlap = threading.Event(), threading.Event(), threading.Event()
                start_cleanup = threading.Barrier(2)
                stats_lock = threading.Lock()
                active = 0
                maximum = 0
                actions, errors, results = [], [], {}
                def call(request, _timeout):
                    nonlocal active, maximum
                    action = request["action"]
                    with stats_lock:
                        active += 1
                        maximum = max(maximum, active)
                        actions.append(action)
                        if active > 1:
                            overlap.set()
                    try:
                        if action != "shutdown":
                            first_entered.set()
                            if not release_first.wait(timeout=2):
                                raise AssertionError("test did not release original mocked request")
                            return {"children": {}}
                        return {"settled": False, "refused": "synthetic original still live"}
                    finally:
                        with stats_lock:
                            active -= 1
                child = FakeChild()
                fixture = SimpleNamespace(layout=SimpleNamespace(name="case-a", runtime=Path("/synthetic/runtime")),
                                          channel=SimpleNamespace(call=call), process=child, pidfd=37)
                def first_worker():
                    try:
                        results["request"] = (fixtures.status(fixture, {}) if operation == "status" else
                                              fixtures.prebuilt_step(fixture, {"step": "daemon-status"}))
                    except BaseException as error:
                        errors.append(error)
                def cleanup_worker():
                    try:
                        start_cleanup.wait(timeout=2)
                        results["cleanup"] = fixtures._kill_holder(fixture)
                    except BaseException as error:
                        errors.append(error)
                workers = [threading.Thread(target=first_worker), threading.Thread(target=cleanup_worker)]
                started = []
                try:
                    workers[0].start()
                    started.append(workers[0])
                    self.assertTrue(first_entered.wait(timeout=2))
                    workers[1].start()
                    started.append(workers[1])
                    start_cleanup.wait(timeout=2)
                    self.assertFalse(overlap.wait(timeout=.1))
                finally:
                    release_first.set()
                    start_cleanup.abort()
                    for worker in started:
                        worker.join(timeout=3)
                    self.assertFalse(any(worker.is_alive() for worker in started), "owned test workers must join")
                self.assertEqual(errors, [])
                self.assertEqual(maximum, 1)
                self.assertEqual(active, 0)
                self.assertEqual(actions, ["status" if operation == "status" else "daemon-status", "shutdown"])
                self.assertFalse(results["cleanup"]["settled"])
                self.assertEqual(child.waits, [])
                child.kill.assert_not_called()
                child.terminate.assert_not_called()


if __name__ == "__main__":
    unittest.main()
