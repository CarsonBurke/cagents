#!/usr/bin/env python3
"""Contracts for the native fixture broker and its agent-side client.

No test here starts a broker through its entry point past the guard, reaches a
daemon, or changes the inherited environment: any real authority token stays
in place, and the isolation proofs run with it. Where CI has no token, children
that must hold one get an inert placeholder.
"""

import importlib.machinery
import importlib.util
import json
import os
from pathlib import Path
import shlex
import shutil
import signal
import socket
import stat
import subprocess
import sys
import tempfile
import threading
import time
import unittest

REPOSITORY = Path(__file__).resolve().parents[1]
BROKER = Path(os.environ.get("NATIVE_FIXTURE_BROKER_UNDER_TEST", REPOSITORY / "scripts/native-fixture-broker"))
CLIENT = REPOSITORY / "scripts/native-fixture"
AUTHORITY = "CAGENTS_AGENT_AUTHORITY"
BWRAP = "/usr/bin/bwrap"
# CI must exercise the sandbox; only an unequipped local host may skip it.
REQUIRE_SANDBOX = os.environ.get("NATIVE_FIXTURE_REQUIRE_SANDBOX") == "1"
INERT = "inert-fixture-placeholder"


def load_broker():
    loader = importlib.machinery.SourceFileLoader("native_fixture_broker", str(BROKER))
    spec = importlib.util.spec_from_loader("native_fixture_broker", loader)
    module = importlib.util.module_from_spec(spec)
    # Dataclasses look their module up while the class is created.
    sys.modules["native_fixture_broker"] = module
    loader.exec_module(module)
    return module


broker = load_broker()


def holding_environment(**extra):
    """The inherited environment, with a token present and any real one unchanged."""
    return {**os.environ, AUTHORITY: os.environ.get(AUTHORITY, INERT), **extra}


def sandbox_usable():
    if not os.access(BWRAP, os.X_OK):
        return False
    return subprocess.run(
        [BWRAP, "--unshare-all", "--ro-bind", "/usr", "/usr", "--symlink", "usr/bin", "/bin",
         "--symlink", "usr/lib64", "/lib64", "--symlink", "usr/lib", "/lib",
         "--", "/usr/bin/true"],
        capture_output=True,
    ).returncode == 0


class TemporaryRepository(unittest.TestCase):
    def setUp(self):
        self._temporary = tempfile.TemporaryDirectory(prefix="nf-")
        self.addCleanup(self._temporary.cleanup)
        self.base = Path(self._temporary.name).resolve()
        self.repo = self.base / "repo"
        (self.repo / ".git").mkdir(parents=True)
        self.checkout = self.make_checkout("wt")

    def make_checkout(self, name):
        checkout = self.repo / ".worktrees" / name
        (checkout / "scripts").mkdir(parents=True)
        (checkout / "tests/fixtures").mkdir(parents=True)
        (checkout / "scripts/run-development").write_text("#!/bin/sh\n")
        (checkout / "tests/fixtures/chat-composer-claude.sh").write_text("#!/bin/sh\n")
        (checkout / "mise.toml").write_text('[tools]\nzig = "0.15.2"\n')
        return checkout


class Guard(unittest.TestCase):
    def run_broker(self, *arguments, **extra):
        with tempfile.TemporaryDirectory(prefix="nf-") as runtime:
            return subprocess.run(
                [sys.executable, "-I", "-B", str(BROKER), *arguments],
                env=holding_environment(XDG_RUNTIME_DIR=runtime, **extra),
                capture_output=True, text=True, timeout=30,
            )

    def test_refuses_inherited_authority_before_reading_arguments(self):
        for arguments in (["--check-guard"], ["--help"], ["--no-such-option"], []):
            with self.subTest(arguments=arguments):
                result = self.run_broker(*arguments)
                self.assertEqual(result.returncode, 77, result.stderr)
                self.assertIn("inherited agent authority was not modified", result.stderr)
                self.assertNotIn(os.environ.get(AUTHORITY) or INERT, result.stderr + result.stdout)

    def test_refused_broker_creates_no_socket(self):
        with tempfile.TemporaryDirectory(prefix="nf-") as runtime:
            subprocess.run(
                [sys.executable, "-I", "-B", str(BROKER)],
                env=holding_environment(XDG_RUNTIME_DIR=runtime),
                capture_output=True, timeout=30,
            )
            self.assertEqual(os.listdir(runtime), [])

    def test_empty_value_and_every_hosted_name_count(self):
        for name in broker.AUTHORITY_NAMES:
            for value in ("", "anything"):
                self.assertEqual(broker.guard({name: value}, parent_pid=1), 77, name)

    def test_clean_process_tree_passes(self):
        self.assertIsNone(broker.guard({"PATH": "/usr/bin"}, parent_pid=1))

    def fake_proc(self, base, chain):
        for pid, parent, environment in chain:
            directory = base / str(pid)
            directory.mkdir()
            (directory / "environ").write_bytes(b"\0".join(environment) + b"\0")
            (directory / "status").write_text(f"Name:\tx\nPPid:\t{parent}\n")

    def test_refuses_an_ancestor_that_held_authority(self):
        with tempfile.TemporaryDirectory(prefix="nf-") as proc:
            proc = Path(proc)
            self.fake_proc(proc, [
                (30, 20, [b"PATH=/usr/bin"]),
                (20, 10, [b"CAGENTS_AGENT_AUTHORITY=value", b"PATH=/usr/bin"]),
                (10, 1, [b"PATH=/usr/bin"]),
            ])
            self.assertEqual(broker.ancestors_holding_authority(30, proc), [20])
            self.assertEqual(broker.guard({}, parent_pid=30, proc_root=proc), 77)

    def test_ancestor_scan_matches_names_rather_than_values(self):
        with tempfile.TemporaryDirectory(prefix="nf-") as proc:
            proc = Path(proc)
            self.fake_proc(proc, [(5, 1, [b"NOTE=CAGENTS_AGENT_AUTHORITY=x"])])
            self.assertEqual(broker.ancestors_holding_authority(5, proc), [])


class Validation(TemporaryRepository):
    def validate(self, **fields):
        return broker.validate_request(fields, self.repo)

    def assert_refused(self, code, **fields):
        with self.assertRaises(broker.Refusal) as caught:
            self.validate(**fields)
        self.assertEqual(caught.exception.code, code, caught.exception.detail)

    def test_accepts_a_checkout_beneath_worktrees(self):
        request = self.validate(op="start", fixture="a-1", checkout=str(self.checkout))
        self.assertEqual(request["checkout"], self.checkout)
        relative = self.validate(op="start", fixture="a-1", checkout="wt")
        self.assertEqual(relative["checkout"], self.checkout)

    def test_refuses_checkout_escapes(self):
        outside = self.base / "outside"
        outside.mkdir()
        link = self.repo / ".worktrees/link"
        link.symlink_to(outside)
        fixture_dir = self.repo / ".worktrees/native-fixture/old"
        fixture_dir.mkdir(parents=True)
        for path in (
            str(outside),
            str(self.repo),
            str(self.repo / ".worktrees"),
            str(self.repo / ".worktrees/../.."),
            "../../outside",
            str(link),
            "/",
            str(fixture_dir),
        ):
            with self.subTest(path=path):
                self.assert_refused("path_escape", op="start", fixture="a", checkout=path)

    def test_refuses_shell_metacharacters_in_named_values(self):
        for name in ("a;b", "a b", "$(x)", "a`b`", "a|b", "../a", "A", "-a", "a\nb", ""):
            with self.subTest(fixture=name):
                self.assert_refused("invalid_request", op="status", fixture=name)
        for name in ("a;b", "a/b", "../x", "$(x)", "-a", "x y", "x.png"):
            with self.subTest(shot=name):
                self.assert_refused("invalid_request", op="screenshot", fixture="a", name=name)
        for chord in ("a;b", "$(x)", "a b", ""):
            with self.subTest(chord=chord):
                self.assert_refused("invalid_request", op="input", fixture="a", action="key", chords=[chord])
        hostile = self.make_checkout("wt;touch x")
        self.assert_refused("invalid_request", op="start", fixture="a", checkout=str(hostile))
        spaced = self.make_checkout("wt $(id)")
        self.assert_refused("invalid_request", op="start", fixture="a", checkout=str(spaced))

    def test_refuses_unknown_operations_and_actions(self):
        for op in ("exec", "shell", "", None, 3, ["start"]):
            with self.subTest(op=op):
                self.assert_refused("unknown_op", op=op, fixture="a")
        self.assert_refused("unknown_action", op="input", fixture="a", action="logs")

    def test_refuses_fields_that_could_change_a_command(self):
        for field in ("env", "cwd", "executable", "command", "shell", "token", "root"):
            with self.subTest(field=field):
                self.assert_refused("invalid_request", op="cli", fixture="a", args=["version"], **{field: "x"})

    def test_cli_arguments_must_be_an_argument_list(self):
        for args in ("project open", [], [1], ["a\0b"], None, ["x"] * 65):
            with self.subTest(args=args):
                self.assert_refused("invalid_request", op="cli", fixture="a", args=args)

    def test_typed_text_stays_one_literal_argument(self):
        text = "$(touch /tmp/x); `id` | -- --flag\n"
        request = self.validate(op="input", fixture="a", action="type", text=text)
        argv = broker.input_argv(request)
        self.assertEqual(argv, ["type", "--", text])

    def test_input_arguments_cannot_become_harness_flags(self):
        click = self.validate(op="input", fixture="a", action="click", target="--no-heal")
        self.assertEqual(broker.input_argv(click), ["click", "--button", "left", "--", "--no-heal"])
        wait = self.validate(op="input", fixture="a", action="wait", text="--gone")
        self.assertEqual(broker.input_argv(wait), ["wait", "--timeout", "20", "--", "--gone"])
        scroll = self.validate(op="input", fixture="a", action="scroll", axis="v", notches=-3)
        self.assertEqual(broker.input_argv(scroll), ["scroll", "--", "v", "-3"])


class Construction(TemporaryRepository):
    def layout(self):
        host = broker.Host(
            repo=self.repo, home=self.base / "home", runtime_dir=self.base / "run",
            cargo_home=self.base / "home/.cargo", rustup_home=self.base / "home/.rustup",
            cargo_target=self.base / "target", mise_data=self.base / "home/.local/share/mise",
        )
        return broker.Layout(host, "demo", self.checkout, self.base / "home/.local/share/mise/installs/zig/0.15.2")

    def bound_sources(self, args):
        sources = []
        for index, argument in enumerate(args):
            if argument in ("--bind", "--ro-bind", "--ro-bind-try", "--dev-bind-try"):
                sources.append(args[index + 1])
        return sources

    def test_real_home_and_runtime_are_never_bound_whole(self):
        layout = self.layout()
        sources = self.bound_sources(broker.sandbox_args(layout, "/wayland"))
        host = layout.host
        for forbidden in (host.home, host.runtime_dir, host.cargo_home, host.home / ".local",
                          host.home / ".local/share", host.home / ".cache", host.repo):
            self.assertNotIn(str(forbidden), sources)
        self.assertIn(str(layout.runtime), sources)

    def test_network_and_namespaces_are_unshared(self):
        args = broker.sandbox_args(self.layout(), "/wayland")
        self.assertIn("--unshare-all", args)
        self.assertNotIn("--share-net", args)
        self.assertIn("--die-with-parent", args)

    def test_checkout_is_read_only(self):
        layout = self.layout()
        args = broker.sandbox_args(layout, "/wayland")
        index = args.index(str(layout.checkout))
        self.assertEqual(args[index - 1], "--ro-bind")

    def test_harness_state_is_not_writable_from_the_sandbox(self):
        layout = self.layout()
        args = broker.sandbox_args(layout, "/wayland")
        self.assertNotIn(str(layout.root), args)
        self.assertNotIn(str(layout.niri_root), args)

    def test_environment_is_built_from_nothing(self):
        layout = self.layout()
        previous = os.environ.get(AUTHORITY)
        os.environ.setdefault(AUTHORITY, INERT)
        try:
            built = broker.sandbox_env(layout)
        finally:
            if previous is None:
                del os.environ[AUTHORITY]
        self.assertFalse([name for name in built if name.startswith("CAGENTS_")])
        self.assertNotIn("DBUS_SESSION_BUS_ADDRESS", built)
        self.assertNotIn("NIRI_SOCKET", built)
        self.assertEqual(built["HOME"], str(layout.sandbox / "home"))
        self.assertEqual(built["XDG_DATA_HOME"], str(layout.sandbox / "data"))
        self.assertEqual(built["WAYLAND_DISPLAY"], "nested-wayland")

    def test_holder_command_quotes_everything_but_the_nested_socket(self):
        layout = self.layout()
        command = broker.holder_command(layout)
        self.assertIn('--bind "$XDG_RUNTIME_DIR/$WAYLAND_DISPLAY" ', command)
        self.assertNotIn("@NESTED_WAYLAND@", command)
        parts = shlex.split(command)
        self.assertEqual(parts[:3], ["exec", "/usr/bin/env", "-i"])
        self.assertEqual(parts[3:7], ["/usr/bin/bwrap", "--new-session", "--info-fd", "3"])
        self.assertEqual(parts[-1], f"3>{layout.info_file}")
        self.assertIn(broker.RELAY, parts)
        self.assertIn(str(layout.checkout), parts)

    def test_runner_argv_is_fixed_and_browserless(self):
        layout = self.layout()
        command = broker.runner_command(layout)
        self.assertEqual(command[0], str(self.checkout / "scripts/run-development"))
        self.assertEqual(command[1], "--browserless")
        self.assertNotIn("--fresh-daemon", command)
        self.assertEqual(command[-1], str(layout.sandbox / "ui.log"))

    def test_harness_session_is_always_named(self):
        argv = broker.harness_argv(self.layout().host, "demo", ["shot", "--", "x"])
        self.assertEqual(argv[1:3], ["--session", "demo"])


class Protocol(TemporaryRepository):
    def setUp(self):
        super().setUp()
        self.runtime = tempfile.TemporaryDirectory(prefix="nf-", dir=None)
        self.addCleanup(self.runtime.cleanup)
        self.path = Path(self.runtime.name) / broker.SOCKET_NAME
        self.seen = []

        def handler(request):
            self.seen.append(request)
            return {"handled": request["op"]}

        self.server = broker.Server(self.path, self.repo, handler)
        thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        thread.start()
        self.addCleanup(thread.join, 5)
        self.addCleanup(self.server.server_close)
        self.addCleanup(self.server.shutdown)

    def exchange(self, raw):
        connection = socket.socket(socket.AF_UNIX)
        connection.connect(str(self.path))
        with connection, connection.makefile("rwb") as stream:
            stream.write(raw)
            stream.flush()
            return json.loads(stream.readline())

    def client(self, *arguments, **extra):
        return subprocess.run(
            [sys.executable, "-I", "-B", str(CLIENT), *arguments],
            env=holding_environment(**{"XDG_RUNTIME_DIR": self.runtime.name, **extra}),
            capture_output=True, text=True, timeout=30,
        )

    def test_socket_is_private(self):
        mode = os.lstat(self.path).st_mode
        self.assertTrue(stat.S_ISSOCK(mode))
        self.assertEqual(stat.S_IMODE(mode), 0o600)

    def test_a_second_broker_does_not_replace_a_live_socket(self):
        with self.assertRaises(broker.Refusal) as caught:
            broker.prepare_socket(self.path)
        self.assertEqual(caught.exception.code, "broker_running")

    def test_a_non_socket_at_the_path_is_left_alone(self):
        other = Path(self.runtime.name) / "other"
        other.write_text("keep")
        with self.assertRaises(broker.Refusal):
            broker.prepare_socket(other)
        self.assertEqual(other.read_text(), "keep")

    def test_malformed_requests_get_structured_refusals(self):
        for raw, code in (
            (b"not json\n", "invalid_request"),
            (b"[]\n", "invalid_request"),
            (b'{"op":"exec","fixture":"a"}\n', "unknown_op"),
            (b'{"op":"status","fixture":"a;b"}\n', "invalid_request"),
            (b'{"op":"start","fixture":"a","checkout":"/etc"}\n', "path_escape"),
            (b'{"op":"cli","fixture":"a","args":["x"],"env":{"A":"b"}}\n', "invalid_request"),
        ):
            with self.subTest(raw=raw):
                reply = self.exchange(raw)
                self.assertFalse(reply["ok"])
                self.assertEqual(reply["code"], code)
        self.assertEqual(self.seen, [])

    def test_an_oversized_request_is_refused(self):
        reply = self.exchange(b'{"op":"status","fixture":"' + b"a" * (broker.MAX_REQUEST_BYTES + 10) + b'"}\n')
        self.assertFalse(reply["ok"])
        self.assertEqual(self.seen, [])

    def test_a_failing_handler_does_not_end_the_broker(self):
        self.server.handler = lambda _request: 1 / 0
        self.assertEqual(self.exchange(b'{"op":"status","fixture":"a"}\n')["code"], "internal_error")
        self.assertEqual(self.exchange(b'{"op":"status","fixture":"a"}\n')["code"], "internal_error")

    def test_client_sends_only_the_request_and_never_the_token(self):
        secret = "client-token-marker"
        raw = []
        original = broker.json.loads

        def capture(data):
            raw.append(data)
            return original(data)

        broker.json.loads = capture
        self.addCleanup(setattr, broker.json, "loads", original)
        # Sets a marker only when no real token is inherited; a real one is used as is.
        token = os.environ.get(AUTHORITY, secret)
        result = self.client("cli", "demo", "--", "project", "open", "/x;y", **{AUTHORITY: token})
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(
            self.seen, [{"op": "cli", "fixture": "demo", "args": ["project", "open", "/x;y"]}]
        )
        sent = raw[0] if isinstance(raw[0], bytes) else raw[0].encode()
        self.assertNotIn(token.encode(), sent)
        self.assertEqual(set(original(sent)), {"op", "fixture", "args"})

    def test_client_requests_match_what_the_broker_accepts(self):
        for arguments in (
            ["run-ui", "demo"],
            ["status", "demo"],
            ["stop", "demo", "--keep"],
            ["screenshot", "demo", "after-chat"],
            ["click", "demo", "Send", "--double", "--button", "right"],
            ["move", "demo", "100,200"],
            ["type", "demo", "hello; $(world)"],
            ["key", "demo", "ctrl+s", "Escape"],
            ["scroll", "demo", "v", "--at", "Send", "--", "-3"],
            ["wait", "demo", "Ready", "--gone", "--timeout", "5"],
            ["observe", "demo", "--no-text"],
            ["start", "demo", "--checkout", str(self.checkout)],
        ):
            with self.subTest(arguments=arguments):
                result = self.client(*arguments)
                self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
                self.assertTrue(json.loads(result.stdout)["ok"], result.stdout)

    def test_client_reports_a_missing_broker_without_starting_one(self):
        with tempfile.TemporaryDirectory(prefix="nf-") as empty:
            result = self.client("status", "demo", XDG_RUNTIME_DIR=empty)
            self.assertEqual(result.returncode, 69)
            self.assertIn("do not start it from this shell", result.stderr)
            self.assertEqual(os.listdir(empty), [])

    def test_client_source_cannot_run_or_forward_anything(self):
        source = CLIENT.read_text()
        for forbidden in ("subprocess", "os.exec", "os.system", "os.spawn", "os.fork",
                          "os.environ.copy", "dict(os.environ)", AUTHORITY):
            self.assertNotIn(forbidden, source)


class FixtureLifecycle(TemporaryRepository):
    """Fixtures logic with niri-harness and sandbox replaced by recording stubs."""

    def fixtures(self):
        host = broker.Host(
            repo=self.repo, home=self.base / "home", runtime_dir=self.base / "run",
            cargo_home=self.base / "home/.cargo", rustup_home=self.base / "home/.rustup",
            cargo_target=self.base / "target", mise_data=self.base / "home/.local/share/mise",
        )
        return broker.Fixtures(host, {"PATH": "/usr/bin"})

    def test_operations_need_a_fixture_this_broker_started(self):
        fixtures = self.fixtures()
        for op in ({"op": "cli", "args": ["x"]}, {"op": "stop"}, {"op": "run-ui"}):
            with self.subTest(op=op), self.assertRaises(broker.Refusal) as caught:
                fixtures.handle({**op, "fixture": "absent"})
            self.assertEqual(caught.exception.code, "unknown_fixture")

    def test_start_requires_the_pinned_zig(self):
        fixtures = self.fixtures()
        with self.assertRaises(broker.Refusal) as caught:
            fixtures.handle({"op": "start", "fixture": "a", "checkout": self.checkout})
        self.assertEqual(caught.exception.code, "missing_toolchain")
        self.assertEqual(fixtures.live, {})

    def test_start_refuses_a_stale_directory_rather_than_deleting_it(self):
        fixtures = self.fixtures()
        zig = fixtures.host.mise_data / "installs/zig/0.15.2"
        zig.mkdir(parents=True)
        (zig / "zig").write_text("")
        stale = self.repo / ".worktrees/native-fixture/a"
        stale.mkdir(parents=True)
        (stale / "evidence.png").write_text("keep")
        with self.assertRaises(broker.Refusal) as caught:
            fixtures.handle({"op": "start", "fixture": "a", "checkout": self.checkout})
        self.assertEqual(caught.exception.code, "fixture_exists")
        self.assertEqual((stale / "evidence.png").read_text(), "keep")
        self.assertEqual(fixtures.live, {})


PROBE = r"""
import json, os, socket, sys
spec = json.loads(sys.argv[1])
report = {
    "visible": [path for path in spec["invisible"] if os.path.lexists(path)],
    "token_present": "CAGENTS_AGENT_AUTHORITY" in os.environ,
    "missing": [path for path in spec["visible"] if not os.path.lexists(path)],
    "writable": [path for path in spec["read_only"] if os.access(path, os.W_OK)],
    "sandbox_writable": os.access(spec["sandbox"], os.W_OK),
    "routes": open("/proc/net/route").read().splitlines()[1:],
    "pid": os.getpid(),
    "home_entries": sorted(os.listdir(spec["home"])) if os.path.isdir(spec["home"]) else [],
}
abstract = socket.socket(socket.AF_UNIX)
try:
    abstract.connect("\0" + spec["abstract"])
    report["abstract_reachable"] = True
except OSError:
    report["abstract_reachable"] = False
wayland = spec.get("wayland")
report["wayland_is_socket"] = bool(wayland) and __import__("stat").S_ISSOCK(os.stat(wayland).st_mode)
print(json.dumps(report))
"""


class Isolation(unittest.TestCase):
    """Real bwrap, run with this process's environment unchanged, token included."""

    @classmethod
    def setUpClass(cls):
        if not sandbox_usable():
            if REQUIRE_SANDBOX:
                raise AssertionError("bubblewrap must be usable where the sandbox is required")
            raise unittest.SkipTest("bubblewrap with user namespaces is unavailable")

    def setUp(self):
        temporary = tempfile.TemporaryDirectory(prefix="nf-")
        self.addCleanup(temporary.cleanup)
        self.base = Path(temporary.name).resolve()
        self.closers = []
        self.addCleanup(lambda: [close() for close in self.closers])

    def listen(self, path):
        server = socket.socket(socket.AF_UNIX)
        server.bind(str(path))
        server.listen(1)
        self.closers.append(server.close)
        return server

    def fixture_layout(self, home, runtime, cargo_home, rustup_home):
        repo = self.base / "repo"
        (repo / ".git").mkdir(parents=True)
        checkout = repo / ".worktrees/wt"
        checkout.mkdir(parents=True)
        (checkout / "marker").write_text("source")
        sibling = repo / ".worktrees/other-agent"
        sibling.mkdir()
        target = self.base / "target"
        target.mkdir()
        zig = self.base / "zig"
        zig.mkdir()
        rustup_home.mkdir(parents=True, exist_ok=True)
        host = broker.Host(repo=repo, home=home, runtime_dir=runtime, cargo_home=cargo_home,
                           rustup_home=rustup_home, cargo_target=target, mise_data=self.base / "mise")
        layout = broker.Layout(host, f"proof{os.getpid()}", checkout, zig)
        for directory in layout.directories:
            directory.mkdir(parents=True, exist_ok=True)
        layout.runtime.mkdir(parents=True)
        self.addCleanup(shutil.rmtree, layout.runtime, ignore_errors=True)
        wayland = self.base / "nested.sock"
        self.listen(wayland)
        return layout, wayland, sibling

    def probe(self, layout, wayland, invisible, visible=(), read_only=()):
        abstract = f"nf-proof-{os.getpid()}-{time.monotonic_ns()}"
        host_abstract = socket.socket(socket.AF_UNIX)
        host_abstract.bind("\0" + abstract)
        host_abstract.listen(1)
        self.closers.append(host_abstract.close)
        spec = {
            "invisible": [str(path) for path in invisible],
            "visible": [str(path) for path in visible],
            "read_only": [str(path) for path in read_only],
            "sandbox": str(layout.sandbox),
            "abstract": abstract,
            "home": str(layout.host.home),
            "wayland": str(layout.nested_wayland),
        }
        # Inherits this process's environment as is: nothing is cleared or unset.
        result = subprocess.run(
            [BWRAP, *broker.sandbox_args(layout, str(wayland)), "--",
             "/usr/bin/python3", "-I", "-c", PROBE, json.dumps(spec)],
            capture_output=True, text=True, timeout=60,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        return json.loads(result.stdout)

    def test_decoy_host_state_is_invisible_and_unreachable(self):
        home = self.base / "home"
        runtime = self.base / "run"
        cargo = home / ".cargo"
        rustup = home / ".rustup"
        invisible = [
            home / ".local/share/cagents/state.db",
            home / ".local/share/cagents-development/state.db",
            home / ".config/cagents/config.toml",
            home / ".ssh/id_ed25519",
            home / ".claude/credentials.json",
            cargo / "credentials.toml",
            runtime / "cagents/cagentsd.sock",
            runtime / "cagents-development/cagentsd.sock",
            runtime / "bus",
            runtime / "systemd/private",
        ]
        for path in invisible:
            path.parent.mkdir(parents=True, exist_ok=True)
            if path.name in ("cagentsd.sock", "bus", "private"):
                self.listen(path)
            else:
                path.write_text("host state")
        (cargo / "registry").mkdir()
        (cargo / "registry/crate").write_text("cached")
        (home / ".cache/zig/p").mkdir(parents=True)
        (home / ".cache/zig/p/pkg").write_text("cached")
        layout, wayland, sibling = self.fixture_layout(home, runtime, cargo, rustup)
        # Positive control: everything asserted absent exists and answers out here.
        probe_socket = socket.socket(socket.AF_UNIX)
        self.closers.append(probe_socket.close)
        probe_socket.connect(str(runtime / "cagents/cagentsd.sock"))
        for path in [*invisible, sibling]:
            self.assertTrue(path.exists(), path)

        report = self.probe(
            layout, wayland,
            invisible=[*invisible, sibling, layout.host.repo / ".git/config", layout.niri_root],
            visible=[layout.checkout / "marker", cargo / "registry/crate",
                     layout.sandbox / "cache/zig/p/pkg", layout.host.cargo_target],
            read_only=[layout.checkout, layout.checkout / "marker", cargo / "registry",
                       layout.sandbox / "cache/zig/p"],
        )
        self.assertEqual(report["visible"], [])
        self.assertEqual(report["missing"], [])
        self.assertEqual(report["writable"], [])
        self.assertTrue(report["sandbox_writable"])
        self.assertEqual(report["routes"], [], "the sandbox must have no route out")
        self.assertFalse(report["abstract_reachable"], "abstract sockets must stay on the host")
        self.assertTrue(report["wayland_is_socket"])
        self.assertLessEqual(set(report["home_entries"]), {".cargo", ".rustup", ".cache", ".local"})

    def real_layout(self):
        home = Path(os.environ["HOME"]).resolve()
        runtime = Path(os.environ.get("XDG_RUNTIME_DIR", f"/run/user/{os.getuid()}"))
        if not runtime.is_dir():
            self.skipTest("no XDG runtime directory on this host")
        # Mount sources only; nothing under them is opened or written.
        layout, wayland, sibling = self.fixture_layout(
            home, runtime,
            Path(os.environ.get("CARGO_HOME", home / ".cargo")),
            Path(os.environ.get("RUSTUP_HOME", home / ".rustup")),
        )
        return layout, wayland, sibling, home, runtime

    def test_real_sockets_and_data_are_invisible_with_token_unchanged(self):
        layout, wayland, _sibling, home, runtime = self.real_layout()
        real = [
            *runtime.glob("cagents*"),
            *(home / ".local/share").glob("cagents*"),
            *(home / ".config").glob("cagents*"),
            runtime / "bus",
            runtime / "systemd/private",
            home / ".ssh",
            home / ".claude",
            home / ".config",
            home / ".local/bin",
            home / "Documents",
            Path(os.environ.get("CARGO_HOME", home / ".cargo")) / "credentials.toml",
        ]
        existing = [path for path in real if os.path.lexists(path)]
        report = self.probe(layout, wayland, invisible=real)
        self.assertEqual(report["visible"], [])
        # Proves the run kept the inherited environment rather than shedding it.
        self.assertEqual(report["token_present"], AUTHORITY in os.environ)
        self.assertEqual(report["routes"], [])
        if AUTHORITY in os.environ:
            # A hosted run can only be on a real host, so the proof is not vacuous.
            self.assertTrue(any("cagents" in path.name for path in existing))

    def holder(self, layout, wayland):
        read_info, write_info = os.pipe()
        # Inherits this process's environment as is; the broker's env -i is not used.
        process = subprocess.Popen(
            [BWRAP, "--info-fd", str(write_info), *broker.sandbox_args(layout, str(wayland)),
             "--", *broker.relay_command(layout.relay_inside)],
            pass_fds=(write_info,),
        )
        os.close(write_info)
        self.addCleanup(process.wait)
        self.addCleanup(process.kill)
        with os.fdopen(read_info) as info:
            pid = json.loads(info.read())["child-pid"]
        deadline = time.monotonic() + 15
        while time.monotonic() < deadline:
            if broker.holder_is_live(layout, pid) and layout.relay_host.exists():
                break
            time.sleep(0.02)
        return process, pid

    def test_commands_share_one_sandbox_and_end_with_its_holder(self):
        home = self.base / "home"
        layout, wayland, _sibling = self.fixture_layout(home, self.base / "run", home / ".cargo", home / ".rustup")
        (home / ".cargo").mkdir(parents=True, exist_ok=True)
        (home / "secret").write_text("host state")
        process, pid = self.holder(layout, wayland)
        self.assertTrue(broker.holder_is_live(layout, pid), "the holder must carry its mark")
        self.assertFalse(broker.holder_is_live(layout, os.getpid()), "another process is not the holder")

        def call(command, **fields):
            # No env field: the relay's child inherits, so nothing here sheds a token.
            return broker.relay_call(
                layout.relay_host,
                {"argv": command, "cwd": str(layout.sandbox / "project"), **fields}, 30,
            )

        script = (
            "import os;"
            f"print(os.getpid(), os.path.exists({str(home / 'secret')!r}), os.getcwd(),"
            f" {AUTHORITY!r} in os.environ, flush=True)"
        )
        first = call(["/usr/bin/python3", "-I", "-c", script], timeout=20)
        self.assertEqual(first["exit_code"], 0, first)
        child_pid, secret_visible, cwd, token = first["stdout"].split()
        self.assertLess(int(child_pid), 100, "the command must run in the sandbox's PID namespace")
        self.assertEqual(secret_visible, "False")
        self.assertEqual(cwd, str(layout.sandbox / "project"))
        self.assertEqual(token == "True", AUTHORITY in os.environ)

        # A daemon started by one command must be seen, and outlive it, for the next.
        call(["/usr/bin/sleep", "300"], background="runner", log=str(layout.sandbox / "bg.log"))
        self.assertEqual(broker.relay_call(layout.relay_host, {"poll": "runner"}, 10)["state"], "running")
        second = call(["/usr/bin/python3", "-I", "-c",
                       "import os;print(sum(open(f'/proc/{p}/comm').read().strip()=='sleep'"
                       " for p in os.listdir('/proc') if p.isdigit()))"], timeout=20)
        self.assertEqual(second["stdout"].strip(), "1", second)

        missing = call(["/no/such/program"], timeout=5)
        self.assertEqual(missing["exit_code"], 127)
        late = call(["/usr/bin/sleep", "30"], timeout=0.2)
        self.assertTrue(late.get("timed_out"))

        os.kill(pid, signal.SIGKILL)
        process.wait(timeout=10)
        self.assertFalse(broker.holder_is_live(layout, pid))
        with self.assertRaises(broker.Refusal):
            broker.relay_call(layout.relay_host, {"poll": "runner"}, 5)

    def test_real_toolchain_runs_inside_the_sandbox(self):
        layout, wayland, _sibling, home, _runtime = self.real_layout()
        cargo = Path(os.environ.get("CARGO_HOME", home / ".cargo")) / "bin/cargo"
        rustup_home = Path(os.environ.get("RUSTUP_HOME", home / ".rustup"))
        if not cargo.exists() or not rustup_home.exists():
            self.skipTest("no rustup-managed Cargo on this host")
        # Only what rustup needs is added; the inherited environment stays intact.
        result = subprocess.run(
            [BWRAP, *broker.sandbox_args(layout, str(wayland)), "--", str(cargo), "--version"],
            env={**os.environ, "RUSTUP_HOME": str(rustup_home),
                 "CARGO_HOME": str(cargo.parents[1]), "RUSTUP_AUTO_INSTALL": "0"},
            capture_output=True, text=True, timeout=60,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertTrue(result.stdout.startswith("cargo "), result.stdout)


if __name__ == "__main__":
    unittest.main()
