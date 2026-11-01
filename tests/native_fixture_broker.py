#!/usr/bin/env python3
"""Contracts for the native fixture broker and its agent-side client.

No test here starts a broker through its entry point past the guard, reaches a
daemon, or changes the inherited environment: any real authority token stays
in place, and every sandbox runs with it. The lifecycle tests replace the
broker's empty sandbox environment with this process's own, and replace
niri-harness and niri with stubs. Where CI has no token, children that must hold
one get an inert placeholder.
"""

import ast
import importlib.machinery
import importlib.util
import io
from contextlib import ExitStack
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
from unittest import mock

REPOSITORY = Path(__file__).resolve().parents[1]
BROKER = Path(os.environ.get("NATIVE_FIXTURE_BROKER_UNDER_TEST", REPOSITORY / "scripts/native-fixture-broker"))
CLIENT = REPOSITORY / "scripts/native-fixture"
AUTHORITY = "CAGENTS_AGENT_AUTHORITY"
BWRAP = "/usr/bin/bwrap"
# CI must exercise the sandbox; only an unequipped local host may skip it.
REQUIRE_SANDBOX = os.environ.get("NATIVE_FIXTURE_REQUIRE_SANDBOX") == "1"
INERT = "inert-fixture-placeholder"
OWNER = "0" * 32


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
    def test_later_operations_need_an_owner_token(self):
        for fields in ({"op": "status"}, {"op": "stop"}, {"op": "run-ui"}):
            with self.subTest(fields=fields):
                self.assert_refused("invalid_request", fixture="a", **fields)
                self.assert_refused("invalid_request", fixture="a", owner="not-hex", **fields)
        self.assertEqual(self.validate(op="status", fixture="a", owner=OWNER)["owner"], OWNER)

    def test_width_must_be_a_whole_number_in_range(self):
        for width in (0, 319, 3841, -1, 800.5, "800", True, None):
            with self.subTest(width=width):
                self.assert_refused("invalid_request", op="viewport", fixture="a", owner=OWNER, width=width)
        self.assert_refused("invalid_request", op="viewport", fixture="a", owner=OWNER)
        self.assertEqual(self.validate(op="viewport", fixture="a", owner=OWNER, width=480)["width"], 480)

    def validate(self, **fields):
        return broker.validate_request(fields, self.repo)

    def assert_refused(self, code, **fields):
        with self.assertRaises(broker.Refusal) as caught:
            self.validate(**fields)
        self.assertEqual(caught.exception.code, code, caught.exception.detail)

    def test_accepts_a_checkout_beneath_worktrees(self):
        request = self.validate(op="start", fixture="a-1", checkout=str(self.checkout))
        self.assertEqual(request["checkout"], self.checkout)
        self.assertEqual(request["width"], broker.DEFAULT_WIDTH)
        relative = self.validate(op="start", fixture="a-1", checkout="wt")
        self.assertEqual(relative["checkout"], self.checkout)

    def test_refuses_checkout_escapes(self):
        outside = self.base / "outside"
        outside.mkdir()
        link = self.repo / ".worktrees/link"
        link.symlink_to(outside)
        fixture_dir = self.repo / ".worktrees/native-fixture/old"
        fixture_dir.mkdir(parents=True)
        target_dir = self.repo / ".worktrees/native-fixture-targets/old"
        target_dir.mkdir(parents=True)
        for path in (
            str(outside),
            str(self.repo),
            str(self.repo / ".worktrees"),
            str(self.repo / ".worktrees/../.."),
            "../../outside",
            str(link),
            "/",
            str(fixture_dir),
            str(target_dir),
        ):
            with self.subTest(path=path):
                self.assert_refused("path_escape", op="start", fixture="a", checkout=path)

    def test_refuses_shell_metacharacters_in_named_values(self):
        for name in ("a;b", "a b", "$(x)", "a`b`", "a|b", "../a", "A", "-a", "a\nb", ""):
            with self.subTest(fixture=name):
                self.assert_refused("invalid_request", op="status", fixture=name, owner=OWNER)
        for name in ("a;b", "a/b", "../x", "$(x)", "-a", "x y", "x.png"):
            with self.subTest(shot=name):
                self.assert_refused("invalid_request", op="screenshot", fixture="a", owner=OWNER, name=name)
        for chord in ("a;b", "$(x)", "a b", ""):
            with self.subTest(chord=chord):
                self.assert_refused("invalid_request", op="input", fixture="a", owner=OWNER, action="key", chords=[chord])
        hostile = self.make_checkout("wt;touch x")
        self.assert_refused("invalid_request", op="start", fixture="a", checkout=str(hostile))
        spaced = self.make_checkout("wt $(id)")
        self.assert_refused("invalid_request", op="start", fixture="a", checkout=str(spaced))

    def test_refuses_unknown_operations_and_actions(self):
        for op in ("exec", "shell", "", None, 3, ["start"]):
            with self.subTest(op=op):
                self.assert_refused("unknown_op", op=op, fixture="a")
        self.assert_refused("unknown_action", op="input", fixture="a", owner=OWNER, action="logs")

    def test_refuses_fields_that_could_change_a_command(self):
        for field in ("env", "cwd", "executable", "command", "shell", "token", "root"):
            with self.subTest(field=field):
                self.assert_refused("invalid_request", op="cli", fixture="a", owner=OWNER, args=["version"], **{field: "x"})

    def test_cli_arguments_must_be_an_argument_list(self):
        for args in ("project open", [], [1], ["a\0b"], None, ["x"] * 65):
            with self.subTest(args=args):
                self.assert_refused("invalid_request", op="cli", fixture="a", owner=OWNER, args=args)

    def test_typed_text_stays_one_literal_argument(self):
        text = "$(touch /tmp/x); `id` | -- --flag\n"
        request = self.validate(op="input", fixture="a", owner=OWNER, action="type", text=text)
        argv = broker.input_argv(request)
        self.assertEqual(argv, ["type", "--", text])

    def test_input_arguments_cannot_become_harness_flags(self):
        click = self.validate(op="input", fixture="a", owner=OWNER, action="click", target="--no-heal")
        self.assertEqual(broker.input_argv(click), ["click", "--button", "left", "--", "--no-heal"])
        wait = self.validate(op="input", fixture="a", owner=OWNER, action="wait", text="--gone")
        self.assertEqual(broker.input_argv(wait), ["wait", "--timeout", "20", "--", "--gone"])
        scroll = self.validate(op="input", fixture="a", owner=OWNER, action="scroll", axis="v", notches=-3)
        self.assertEqual(broker.input_argv(scroll), ["scroll", "--", "v", "-3"])


def make_host(base, repo):
    home = base / "home"
    return broker.Host(
        repo=repo, home=home, runtime_dir=base / "run", cargo_home=home / ".cargo",
        rustup_home=home / ".rustup", mise_data=home / ".local/share/mise",
    )


class Construction(TemporaryRepository):
    def layout(self, checkout=None):
        host = make_host(self.base, self.repo)
        return broker.Layout(host, "demo", checkout or self.checkout, host.mise_data / "installs/zig/0.15.2")

    def args(self, layout=None):
        return broker.sandbox_args(layout or self.layout(), broker.Binds(11, 12, 13, 14, 15))

    def bound_sources(self, args):
        sources = []
        for index, argument in enumerate(args):
            if argument in ("--bind", "--ro-bind", "--ro-bind-try", "--dev-bind-try"):
                sources.append(args[index + 1])
        return sources

    def test_real_home_and_runtime_are_never_bound_whole(self):
        layout = self.layout()
        sources = self.bound_sources(self.args(layout))
        host = layout.host
        for forbidden in (host.home, host.runtime_dir, host.cargo_home, host.home / ".local",
                          host.home / ".local/share", host.home / ".cache", host.repo):
            self.assertNotIn(str(forbidden), sources)

    def test_network_and_namespaces_are_unshared_and_capabilities_dropped(self):
        args = self.args()
        self.assertIn("--unshare-all", args)
        self.assertNotIn("--share-net", args)
        self.assertIn("--die-with-parent", args)
        self.assertEqual(args[args.index("--cap-drop") + 1], "ALL")

    def test_root_is_remounted_read_only_after_every_mount(self):
        args = self.args()
        self.assertEqual(args[-2:], ["--remount-ro", "/"])

    def test_changeable_sources_are_mounted_from_held_descriptors(self):
        layout = self.layout()
        args = self.args(layout)
        for flag, descriptor, destination in (
            ("--ro-bind-fd", "11", layout.checkout),
            ("--bind-fd", "12", layout.target),
            ("--bind-fd", "13", layout.sandbox),
            ("--bind-fd", "14", layout.host.runtime_dir),
            ("--bind-fd", "15", layout.nested_wayland),
        ):
            index = args.index(str(destination))
            self.assertEqual(args[index - 2:index], [flag, descriptor])

    def test_cargo_target_belongs_to_the_broker_and_the_checkout(self):
        layout = self.layout()
        self.assertEqual(layout.target.parents[1], self.repo / ".worktrees")
        self.assertEqual(layout.target.parent.name, broker.TARGETS_DIR)
        self.assertEqual(broker.validate_target(layout), layout.target)
        other = self.layout(self.make_checkout("other"))
        self.assertNotEqual(layout.target, other.target)
        self.assertEqual(broker.sandbox_env(layout)["CARGO_TARGET_DIR"], str(layout.target))
        self.assertNotIn(str(self.repo / "target"), self.args(layout))

    def test_a_target_outside_the_brokers_area_is_refused(self):
        class Elsewhere(broker.Layout):
            target = property(lambda self: self.host.repo / "target")
        layout = self.layout()
        strayed = Elsewhere(layout.host, layout.name, layout.checkout, layout.zig_dir)
        with self.assertRaises(broker.Refusal) as caught:
            broker.validate_target(strayed)
        self.assertEqual(caught.exception.code, "invalid_target")

    def test_the_broker_has_no_option_or_variable_selecting_a_target(self):
        source = BROKER.read_text()
        self.assertNotIn("--cargo-target-dir", source)
        self.assertNotIn('environ.get("CARGO_TARGET_DIR")', source)

    def test_harness_state_is_outside_the_sandbox_and_the_fixture_root(self):
        layout = self.layout()
        for path in (layout.niri_root, layout.harness_state):
            self.assertIn(layout.host.runtime_dir, path.parents)
            self.assertNotIn(layout.root, path.parents)
            self.assertNotIn(layout.runtime, path.parents)
        self.assertNotIn(str(layout.root), self.args(layout))

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
        self.assertEqual(built["WAYLAND_DISPLAY"], "nested-wayland")

    def test_the_real_sandbox_starts_from_an_empty_environment(self):
        self.assertEqual(broker.Fixtures.holder_environment, {})
        self.assertIs(broker.Fixtures.environment_for, broker.sandbox_env)

    def test_holder_applies_limits_and_inherits_its_channel(self):
        layout = self.layout()
        argv = broker.holder_argv(layout, broker.Binds(11, 12, 13, 14, 15), 7, 8)
        self.assertEqual(argv[0], "/usr/bin/prlimit")
        for limit in ("--nproc=", "--core=", "--fsize=", "--as="):
            self.assertTrue([part for part in argv if part.startswith(limit)], limit)
        self.assertEqual(argv[argv.index("--info-fd") + 1], "7")
        self.assertEqual(argv[-1], "8")
        self.assertIn(broker.RELAY, argv)

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

    def test_termination_covers_a_closed_terminal(self):
        previous = {number: signal.getsignal(number) for number in broker.TERMINATION_SIGNALS}
        self.assertIn(signal.SIGHUP, previous)
        self.addCleanup(lambda: [signal.signal(n, h) for n, h in previous.items()])
        broker.install_termination()
        with self.assertRaises(KeyboardInterrupt):
            os.kill(os.getpid(), signal.SIGHUP)
            time.sleep(5)

    def test_focus_returns_only_to_a_window_that_still_exists(self):
        nested = {"id": 9, "is_focused": True}
        other = {"id": 2, "is_focused": False}
        previous = {"id": 1, "is_focused": False}
        self.assertEqual(broker.focus_to_restore(1, {1, 2}, [previous, other, nested]), 1)
        self.assertIsNone(broker.focus_to_restore(1, {1, 2}, [other, nested]), "stale window")
        self.assertIsNone(broker.focus_to_restore(None, {1}, [previous, nested]))
        moved = [previous, {"id": 2, "is_focused": True}]
        self.assertIsNone(broker.focus_to_restore(1, {1, 2}, moved), "the person moved focus")

    def test_scale_gives_the_requested_width_within_range(self):
        self.assertEqual(broker.scale_for_width(1600, 800), 2.0)
        self.assertEqual(broker.scale_for_width(1600, 3200), 0.5)
        self.assertEqual(broker.scale_for_width(1600, 320), 4.0)

    def test_a_binary_must_come_from_this_checkout(self):
        dep = b"/target/cagents: /repo/.worktrees/wt/src/main.rs\n"
        self.assertTrue(broker.binary_belongs_to(dep, Path("/repo/.worktrees/wt")))
        self.assertFalse(broker.binary_belongs_to(dep, Path("/repo/.worktrees/other")))
        self.assertFalse(broker.binary_belongs_to(dep, Path("/repo/.worktrees/w")))
        self.assertFalse(broker.binary_belongs_to(None, Path("/repo/.worktrees/wt")))


class HeldPaths(TemporaryRepository):
    def test_hold_refuses_a_symlink_at_any_level(self):
        elsewhere = self.base / "elsewhere"
        (elsewhere / "inner").mkdir(parents=True)
        (self.repo / ".worktrees/link").symlink_to(elsewhere)
        for parts in ((".worktrees", "link"), (".worktrees", "link", "inner")):
            with self.subTest(parts=parts), self.assertRaises(broker.Refusal) as caught:
                broker.hold_directory(self.repo, parts)
            self.assertEqual(caught.exception.code, "path_escape")
        with self.assertRaises(broker.Refusal):
            broker.hold_directory(self.repo, (".worktrees", "link", "made"), create=True)
        self.assertFalse((elsewhere / "made").exists())

    def test_removal_refuses_a_symlinked_parent(self):
        victim = self.base / "victim"
        (victim / "name").mkdir(parents=True)
        (victim / "name/keep").write_text("keep")
        (self.repo / ".worktrees/native-fixture").symlink_to(victim)
        self.assertFalse(broker.remove_beneath(self.repo, (".worktrees", "native-fixture", "name")))
        self.assertEqual((victim / "name/keep").read_text(), "keep")

    def test_removal_does_not_follow_a_planted_link(self):
        victim = self.base / "victim"
        victim.mkdir()
        (victim / "keep").write_text("keep")
        fixture = self.repo / ".worktrees/native-fixture/name"
        fixture.mkdir(parents=True)
        (fixture / "planted").symlink_to(victim)
        self.assertTrue(broker.remove_beneath(self.repo, (".worktrees", "native-fixture", "name")))
        self.assertFalse(fixture.exists())
        self.assertEqual((victim / "keep").read_text(), "keep")

    def test_reading_refuses_links_at_every_level(self):
        (self.base / "real").mkdir()
        (self.base / "real/file").write_text("data")
        (self.base / "real/link").symlink_to(self.base / "real/file")
        (self.base / "linked").symlink_to(self.base / "real")
        self.assertEqual(broker.read_regular(self.base, ("real",), "file", 2), b"da")
        self.assertIsNone(broker.read_regular(self.base, ("real",), "link", 10))
        self.assertIsNone(broker.read_regular(self.base, ("linked",), "file", 10))
        self.assertIsNone(broker.read_regular(self.base, ("absent",), "file", 10))

    def test_reading_a_fifo_is_refused_within_a_time_limit(self):
        os.mkfifo(self.base / "fifo")
        result = []
        thread = threading.Thread(
            target=lambda: result.append(broker.read_regular(self.base, (), "fifo", 10)), daemon=True,
        )
        thread.start()
        thread.join(5)
        self.assertFalse(thread.is_alive(), "reading a FIFO must not block the broker")
        self.assertEqual(result, [None])

    def trap(self, root, victim):
        """What sandbox code can leave: revoked modes, nested, around a planted link."""
        (root / "trap/deeper").mkdir(parents=True)
        (root / "trap/deeper/file").write_text("x")
        (root / "trap/deeper/link").symlink_to(victim)
        (root / "trap/deeper").chmod(0)
        (root / "trap").chmod(0)

    def test_removal_repairs_revoked_modes_without_following_links(self):
        victim = self.base / "victim"
        victim.mkdir(mode=0o755)
        (victim / "keep").write_text("keep")
        fixture = self.repo / ".worktrees/native-fixture/name"
        fixture.mkdir(parents=True)
        self.trap(fixture, victim)
        fixture.chmod(0)
        self.assertTrue(broker.remove_beneath(self.repo, (".worktrees", "native-fixture", "name")))
        self.assertFalse(os.path.lexists(fixture))
        self.assertEqual((victim / "keep").read_text(), "keep")
        self.assertEqual(stat.S_IMODE(victim.stat().st_mode), 0o755)

    def test_removal_reports_rather_than_raises_when_it_cannot(self):
        fixture = self.repo / ".worktrees/native-fixture/name"
        fixture.mkdir(parents=True)
        original = broker._remove_entry

        def fails(_parent, _name):
            raise PermissionError("resisted")

        broker._remove_entry = fails
        self.addCleanup(setattr, broker, "_remove_entry", original)
        self.assertFalse(broker.remove_beneath(self.repo, (".worktrees", "native-fixture", "name")))


class Relay(unittest.TestCase):
    """The broker's end of the channel against a hostile sandbox."""

    def setUp(self):
        self.ours, self.theirs = socket.socketpair()
        self.addCleanup(self.theirs.close)
        self.channel = broker.RelayChannel(self.ours)
        self.addCleanup(self.channel.close)

    def refused(self, timeout=5):
        with self.assertRaises(broker.Refusal) as caught:
            self.channel.call({"poll": "runner"}, timeout)
        return caught.exception.code

    def test_an_endless_reply_is_cut_off(self):
        original = broker.MAX_RELAY_REPLY_BYTES
        broker.MAX_RELAY_REPLY_BYTES = 4096
        self.addCleanup(setattr, broker, "MAX_RELAY_REPLY_BYTES", original)
        self.theirs.sendall(b"x" * 8192)
        self.assertEqual(self.refused(), "sandbox_failed")
        self.assertLessEqual(len(self.channel.buffer), 65536 + 8192)

    def test_a_trickle_cannot_outlast_the_total_deadline(self):
        def trickle():
            for _ in range(100):
                try:
                    self.theirs.sendall(b"x")
                except OSError:
                    return
                time.sleep(0.05)
        thread = threading.Thread(target=trickle, daemon=True)
        thread.start()
        started = time.monotonic()
        self.assertEqual(self.refused(timeout=0.3), "timeout")
        self.assertLess(time.monotonic() - started, 2)

    def test_a_silent_sandbox_times_out(self):
        self.assertEqual(self.refused(timeout=0.1), "timeout")

    def test_malformed_and_mismatched_replies_are_refused_and_end_the_channel(self):
        for reply in (b"not json\n", b"[1]\n", b'{"id": 99, "state": "running"}\n'):
            with self.subTest(reply=reply):
                ours, theirs = socket.socketpair()
                self.addCleanup(theirs.close)
                channel = broker.RelayChannel(ours)
                self.addCleanup(channel.close)
                theirs.sendall(reply)
                with self.assertRaises(broker.Refusal) as caught:
                    channel.call({"poll": "runner"}, 5)
                self.assertEqual(caught.exception.code, "sandbox_failed")
                with self.assertRaises(broker.Refusal):
                    channel.call({"poll": "runner"}, 5)

    def test_forged_result_fields_are_refused(self):
        for reply in ({"exit_code": "0", "stdout": "", "stderr": ""}, {"exit_code": True, "stdout": "", "stderr": ""},
                      {"exit_code": 0, "stdout": 1, "stderr": ""}, {}):
            with self.subTest(reply=reply), self.assertRaises(broker.Refusal):
                broker.command_result(reply)
        self.assertEqual(broker.command_result({"exit_code": 3, "stdout": "o", "stderr": "e", "x": 1}),
                         {"exit_code": 3, "stdout": "o", "stderr": "e"})
        huge = broker.command_result({"exit_code": 0, "stdout": "x" * (broker.MAX_OUTPUT_BYTES * 2), "stderr": ""})
        self.assertEqual(len(huge["stdout"]), broker.MAX_OUTPUT_BYTES)


PROBE = r"""
import json, os, resource, socket, sys
spec = json.loads(sys.argv[1])
def writable(path):
    # Used for directories only.
    probe = os.path.join(path, ".probe")
    try:
        with open(probe, "w"):
            pass
        os.unlink(probe)
        return True
    except OSError:
        return False
with open("/proc/self/status") as status:
    capabilities = [line.split()[1] for line in status if line.startswith("CapEff")][0]
report = {
    "visible": [path for path in spec["invisible"] if os.path.lexists(path)],
    "token_present": "CAGENTS_AGENT_AUTHORITY" in os.environ,
    "missing": [path for path in spec["visible"] if not os.path.lexists(path)],
    "writable": [path for path in spec["read_only"] if writable(path)],
    "sandbox_writable": writable(spec["sandbox"]),
    "root_writable": writable("/"),
    "cargo_home_writable": writable(spec["cargo_home"]),
    "capabilities": capabilities,
    "routes": open("/proc/net/route").read().splitlines()[1:],
    "home_entries": sorted(os.listdir(spec["home"])) if os.path.isdir(spec["home"]) else [],
    "descriptors": sorted(os.readlink(f"/proc/self/fd/{fd}") for fd in os.listdir("/proc/self/fd")
                          if os.path.exists(f"/proc/self/fd/{fd}")),
    "markers": {path: open(path).read() for path in spec.get("read", [])},
    "nofile_nproc": resource.getrlimit(resource.RLIMIT_NPROC)[0],
}
abstract = socket.socket(socket.AF_UNIX)
try:
    abstract.connect("\0" + spec["abstract"])
    report["abstract_reachable"] = True
except OSError:
    report["abstract_reachable"] = False
report["wayland_is_socket"] = __import__("stat").S_ISSOCK(os.stat(spec["wayland"]).st_mode)
print(json.dumps(report))
"""


class SandboxCase(unittest.TestCase):
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


class Isolation(SandboxCase):
    """Real bwrap, run with this process's environment unchanged, token included."""

    def fixture_layout(self, home, runtime, cargo_home, rustup_home):
        repo = self.base / "repo"
        (repo / ".git").mkdir(parents=True)
        checkout = repo / ".worktrees/wt"
        checkout.mkdir(parents=True)
        (checkout / "marker").write_text("source")
        sibling = repo / ".worktrees/other-agent"
        sibling.mkdir()
        zig = self.base / "zig"
        zig.mkdir()
        rustup_home.mkdir(parents=True, exist_ok=True)
        host = broker.Host(repo=repo, home=home, runtime_dir=runtime, cargo_home=cargo_home,
                           rustup_home=rustup_home, mise_data=self.base / "mise")
        layout = broker.Layout(host, f"proof{os.getpid()}", checkout, zig)
        layout.target.mkdir(parents=True)
        for name in layout.sandbox_directories:
            (layout.sandbox / name).mkdir(parents=True, exist_ok=True)
        layout.niri_root.mkdir(parents=True, exist_ok=True) if runtime.parent == self.base else None
        layout.runtime.mkdir(parents=True)
        self.addCleanup(shutil.rmtree, layout.runtime, ignore_errors=True)
        directory = self.base / "nested"
        directory.mkdir()
        self.listen(directory / "wayland-1")
        return layout, directory, sibling

    def probe(self, layout, directory, invisible, visible=(), read_only=(), read=()):
        abstract = f"nf-proof-{os.getpid()}-{time.monotonic_ns()}"
        host_abstract = socket.socket(socket.AF_UNIX)
        host_abstract.bind("\0" + abstract)
        host_abstract.listen(1)
        self.closers.append(host_abstract.close)
        spec = {
            "invisible": [str(path) for path in invisible],
            "visible": [str(path) for path in visible],
            "read_only": [str(path) for path in read_only],
            "read": [str(path) for path in read],
            "sandbox": str(layout.sandbox),
            "abstract": abstract,
            "home": str(layout.host.home),
            "cargo_home": str(layout.host.cargo_home),
            "wayland": str(layout.nested_wayland),
        }
        binds = broker.hold_binds(layout, directory, "wayland-1")
        self.addCleanup(binds.close)
        # Inherits this process's environment as is: nothing is cleared or unset.
        result = subprocess.run(
            [BWRAP, *broker.sandbox_args(layout, binds), "--",
             "/usr/bin/python3", "-I", "-c", PROBE, json.dumps(spec)],
            capture_output=True, text=True, timeout=60, pass_fds=binds.fds(),
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
        layout, directory, sibling = self.fixture_layout(home, runtime, cargo, rustup)
        repo_target = layout.host.repo / "target"
        repo_target.mkdir()
        # Positive control: everything asserted absent exists and answers out here.
        probe_socket = socket.socket(socket.AF_UNIX)
        self.closers.append(probe_socket.close)
        probe_socket.connect(str(runtime / "cagents/cagentsd.sock"))
        for path in [*invisible, sibling, repo_target]:
            self.assertTrue(path.exists(), path)

        report = self.probe(
            layout, directory,
            invisible=[*invisible, sibling, repo_target, layout.host.repo / ".git/config",
                       layout.niri_root, layout.private],
            visible=[layout.checkout / "marker", cargo / "registry/crate",
                     layout.sandbox / "cache/zig/p/pkg", layout.target],
            read_only=[layout.checkout, cargo / "registry", layout.sandbox / "cache/zig/p"],
        )
        self.assertEqual(report["visible"], [])
        self.assertEqual(report["missing"], [])
        self.assertEqual(report["writable"], [])
        self.assertTrue(report["sandbox_writable"])
        self.assertFalse(report["root_writable"], "nothing may be planted at the root")
        self.assertTrue(report["cargo_home_writable"], "Cargo needs its lock file there")
        self.assertEqual(int(report["capabilities"], 16), 0)
        self.assertEqual(report["routes"], [], "the sandbox must have no route out")
        self.assertFalse(report["abstract_reachable"], "abstract sockets must stay on the host")
        self.assertTrue(report["wayland_is_socket"])
        self.assertLessEqual(set(report["home_entries"]), {".cargo", ".rustup", ".cache", ".local"})
        leaked = [path for path in report["descriptors"]
                  if path.startswith(("/repo", str(layout.host.repo))) and "wt" not in path]
        self.assertEqual(leaked, [])

    def test_a_swapped_path_cannot_change_what_is_mounted(self):
        home = self.base / "home"
        layout, directory, _sibling = self.fixture_layout(home, self.base / "run", home / ".cargo", home / ".rustup")
        (home / ".cargo").mkdir(parents=True, exist_ok=True)
        binds = broker.hold_binds(layout, directory, "wayland-1")
        self.addCleanup(binds.close)
        decoy = self.base / "decoy"
        decoy.mkdir()
        (decoy / "marker").write_text("swapped")
        layout.checkout.rename(self.base / "moved")
        layout.checkout.symlink_to(decoy)
        result = subprocess.run(
            [BWRAP, *broker.sandbox_args(layout, binds), "--", "/usr/bin/cat", str(layout.checkout / "marker")],
            capture_output=True, text=True, timeout=30, pass_fds=binds.fds(),
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout, "source")

    def real_layout(self):
        home = Path(os.environ["HOME"]).resolve()
        runtime = Path(os.environ.get("XDG_RUNTIME_DIR", f"/run/user/{os.getuid()}"))
        if not runtime.is_dir():
            self.skipTest("no XDG runtime directory on this host")
        # Mount sources only; nothing under them is opened or written.
        layout, directory, sibling = self.fixture_layout(
            home, runtime,
            Path(os.environ.get("CARGO_HOME", home / ".cargo")),
            Path(os.environ.get("RUSTUP_HOME", home / ".rustup")),
        )
        return layout, directory, sibling, home, runtime

    def test_real_sockets_and_data_are_invisible_with_token_unchanged(self):
        layout, directory, _sibling, home, runtime = self.real_layout()
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
        report = self.probe(layout, directory, invisible=real)
        self.assertEqual(report["visible"], [])
        # Proves the run kept the inherited environment rather than shedding it.
        self.assertEqual(report["token_present"], AUTHORITY in os.environ)
        self.assertEqual(report["routes"], [])
        # The decoy test's paths can sit under /tmp's tmpfs; this one is on the read-only root.
        self.assertFalse(report["root_writable"])
        self.assertTrue(report["cargo_home_writable"], "Cargo needs its lock file there")
        if AUTHORITY in os.environ:
            # A hosted run can only be on a real host, so the proof is not vacuous.
            self.assertTrue(any("cagents" in path.name for path in existing))

    def test_real_toolchain_runs_inside_the_sandbox(self):
        layout, directory, _sibling, home, _runtime = self.real_layout()
        cargo = Path(os.environ.get("CARGO_HOME", home / ".cargo")) / "bin/cargo"
        rustup_home = Path(os.environ.get("RUSTUP_HOME", home / ".rustup"))
        if not cargo.exists() or not rustup_home.exists():
            self.skipTest("no rustup-managed Cargo on this host")
        binds = broker.hold_binds(layout, directory, "wayland-1")
        self.addCleanup(binds.close)
        # Only what rustup needs is added; the inherited environment stays intact.
        result = subprocess.run(
            [BWRAP, *broker.sandbox_args(layout, binds), "--", str(cargo), "--version"],
            env={**os.environ, "RUSTUP_HOME": str(rustup_home),
                 "CARGO_HOME": str(cargo.parents[1]), "RUSTUP_AUTO_INSTALL": "0"},
            capture_output=True, text=True, timeout=60, pass_fds=binds.fds(),
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertTrue(result.stdout.startswith("cargo "), result.stdout)


STUB_HARNESS = r"""#!/bin/sh
printf '%s\n' "$*" >> "$STUB_LOG"
case "$1 $2" in
  "session start")
    root=
    while [ "$#" -gt 0 ]; do
      [ "$1" = "--root" ] && root=$2
      shift
    done
    mkdir -p "$root"
    printf 'WAYLAND_DISPLAY=wayland-9\nNIRI_SOCKET=/nested.sock\n' > "$root/session.env"
    [ -n "$STUB_START_FAILS" ] && exit 3
    exit 0 ;;
  "session stop") exit 0 ;;
esac
case "$*" in
  *" shot -- "*)
    for name; do :; done
    mkdir -p "$STUB_SHOTS"
    printf 'image' > "$STUB_SHOTS/$name.png" ;;
esac
printf '{"driven": true}\n'
"""
STUB_NIRI = r"""#!/usr/bin/python3 -I
import json, os, sys, time
args = sys.argv[1:]
with open(os.environ["STUB_LOG"], "a") as log:
    log.write(f"NIRI_SOCKET={os.environ.get('NIRI_SOCKET')} {' '.join(args)}\n")
state = os.environ["STUB_STATE"]
def load():
    try:
        with open(state) as stream:
            return json.load(stream)
    except OSError:
        return {"physical": 1600, "scale": 1.0, "next": None, "at": 0}
current = load()
if current["next"] is not None and time.time() >= current["at"]:
    current.update(scale=current["next"], next=None)
if args == ["msg", "--json", "outputs"]:
    width = round(current["physical"] / current["scale"])
    print(json.dumps({"Nested": {"logical": {"width": width, "height": 600, "scale": current["scale"]}}}))
elif args == ["msg", "--json", "windows"]:
    print("[]")
elif args == ["msg", "--json", "focused-window"]:
    print("null")
elif args[:2] == ["msg", "output"]:
    current.update(next=float(args[-1]), at=time.time() + float(os.environ.get("STUB_SCALE_DELAY", "0")))
with open(state, "w") as stream:
    json.dump(current, stream)
"""
STUB_RUNNER = r"""#!/bin/sh
mkdir -p "$CARGO_TARGET_DIR/dev-runtime"
self=$(dirname "$(dirname "$0")")
printf '#!/bin/sh\nprintf "stub cli: %%s %%s\\n" "$(pwd)" "$*"\n' > "$CARGO_TARGET_DIR/dev-runtime/cagents"
chmod +x "$CARGO_TARGET_DIR/dev-runtime/cagents"
printf '%s: %s/src/main.rs\n' "$CARGO_TARGET_DIR/dev-runtime/cagents" "$self" > "$CARGO_TARGET_DIR/dev-runtime/cagents.d"
[ -e "$HOME/fail-build" ] && exit 1
while [ -e "$HOME/hold-build" ]; do /usr/bin/sleep 0.02; done
printf 'writing UI diagnostics to %s\n' ui.log
exec /usr/bin/sleep 300
"""


class Lifecycle(SandboxCase):
    """Fixtures against real bwrap, with niri-harness and niri stubbed.

    The broker's sandbox environment is replaced so this process's own passes
    through unchanged, token included: nothing here runs without it.
    """

    def setUp(self):
        super().setUp()
        self.repo = self.base / "repo"
        (self.repo / ".git").mkdir(parents=True)
        self.checkout = self.make_checkout("wt")
        home = self.base / "home"
        host = broker.Host(
            repo=self.repo, home=home, runtime_dir=self.base / "run", cargo_home=home / ".cargo",
            rustup_home=home / ".rustup", mise_data=home / ".local/share/mise",
            harness=str(self.stub("harness", STUB_HARNESS)), niri=str(self.stub("niri", STUB_NIRI)),
        )
        for path in (host.runtime_dir, host.cargo_home, host.rustup_home, host.mise_data / "installs/zig/0.15.2"):
            path.mkdir(parents=True)
        (host.mise_data / "installs/zig/0.15.2/zig").write_text("")
        self.listen(host.runtime_dir / "wayland-9")
        self.log = self.base / "stub.log"
        self.harness_env = {
            **os.environ, "STUB_LOG": str(self.log), "STUB_STATE": str(self.base / "niri.json"),
            "STUB_SHOTS": str(host.runtime_dir / broker.PRIVATE_DIR / "demo/niri/shots"),
        }
        self.fixtures = broker.Fixtures(host, self.harness_env)
        self.fixtures.holder_environment = None
        self.fixtures.environment_for = lambda layout: {**os.environ, **broker.sandbox_env(layout)}
        self.addCleanup(self.fixtures.stop_all)
        self.host = host

    def stub(self, name, source):
        path = self.base / name
        path.write_text(source)
        path.chmod(0o755)
        return path

    def make_checkout(self, name):
        checkout = self.repo / ".worktrees" / name
        (checkout / "scripts").mkdir(parents=True)
        (checkout / "tests/fixtures").mkdir(parents=True)
        runner = checkout / "scripts/run-development"
        runner.write_text(STUB_RUNNER)
        runner.chmod(0o755)
        (checkout / "tests/fixtures/chat-composer-claude.sh").write_text("#!/bin/sh\n")
        (checkout / "mise.toml").write_text('[tools]\nzig = "0.15.2"\n')
        return checkout

    def request(self, op, **fields):
        return broker.validate_request({"op": op, **fields}, self.repo)

    def call(self, op, **fields):
        return self.fixtures.handle(self.request(op, **fields))

    def start(self, name="demo", checkout="wt", **fields):
        return self.call("start", fixture=name, checkout=checkout, **fields)

    def refused(self, code, op, **fields):
        with self.assertRaises(broker.Refusal) as caught:
            self.call(op, **fields)
        self.assertEqual(caught.exception.code, code, caught.exception.detail)

    def alive(self, pid):
        try:
            state = Path(f"/proc/{pid}/stat").read_text().rpartition(")")[2].split()[0]
        except OSError:
            return False
        return state != "Z"

    def await_gone(self, pid, seconds=10):
        deadline = time.monotonic() + seconds
        while self.alive(pid) and time.monotonic() < deadline:
            time.sleep(0.02)
        return not self.alive(pid)

    def await_built(self, name, owner):
        deadline = time.monotonic() + 15
        while time.monotonic() < deadline:
            if self.call("status", fixture=name, owner=owner)["cli_built"]:
                return
            time.sleep(0.05)
        self.fail("the stub runner did not build")

    def test_the_whole_flow_then_stop_ends_the_sandbox_and_frees_the_name(self):
        started = self.start(width=800)
        owner = started["owner"]
        self.assertTrue(broker.OWNER_TOKEN.fullmatch(owner))
        self.assertEqual(started["target"], str(self.fixtures.live["demo"].layout.target))
        self.assertEqual(started["viewport"]["requested_width"], 800)
        self.assertTrue(started["viewport"]["matches"])
        log = self.log.read_text()
        self.assertIn("NIRI_SOCKET=/nested.sock msg output Nested scale 2.0", log)
        self.assertEqual(started["viewport"]["width"], 800)
        self.assertIn("--cmd /usr/bin/true", log, "nested niri must run nothing from a checkout")
        fixture = self.fixtures.live["demo"]
        holder = fixture.holder_pid

        self.refused("wrong_owner", "status", fixture="demo", owner="f" * 32)
        self.refused("cli_not_built", "cli", fixture="demo", owner=owner, args=["version"])
        self.assertEqual(self.call("status", fixture="demo", owner=owner)["runner"], "not_started")

        self.call("run-ui", fixture="demo", owner=owner)
        self.await_built("demo", owner)
        self.refused("runner_active", "run-ui", fixture="demo", owner=owner)
        output = self.call("cli", fixture="demo", owner=owner, args=["project", "open", "$(x); y"])
        self.assertEqual(output["exit_code"], 0, output)
        self.assertIn("project open $(x); y", output["stdout"])
        self.assertIn(str(fixture.layout.sandbox / "project"), output["stdout"])

        typed = self.call("input", fixture="demo", owner=owner, action="type", text="a; b")
        self.assertEqual(typed["exit_code"], 0, typed)
        self.assertIn("--session demo --json type -- a; b", self.log.read_text())
        shot = self.call("screenshot", fixture="demo", owner=owner, name="after-narrow")
        self.assertEqual(Path(shot["path"]).read_bytes(), b"image")
        self.assertEqual(Path(shot["path"]).parent, Path(started["shots"]))
        self.assertEqual(shot["viewport"]["width"], 800)
        self.assertTrue(shot["viewport"]["matches"])

        wide = self.call("viewport", fixture="demo", owner=owner, width=1600)
        self.assertEqual(wide["requested_width"], 1600)
        self.assertTrue(wide["matches"])
        self.assertIn("msg output Nested scale 1.0", self.log.read_text())
        # 1600 physical pixels cannot reach 320 logical at niri's largest scale of 4.
        narrow = self.call("viewport", fixture="demo", owner=owner, width=320)
        self.assertFalse(narrow["matches"], "a width that was not reached must say so")
        self.assertEqual(narrow["width"], 400)

        stopped = self.call("stop", fixture="demo", owner=owner)
        self.assertTrue(stopped["removed"])
        self.assertTrue(self.await_gone(holder), "stop must end the sandbox")
        self.assertIn("session stop demo", self.log.read_text())
        self.assertFalse(Path(started["root"]).exists())
        self.assertFalse(fixture.layout.runtime.exists())
        self.assertFalse(fixture.layout.private.exists())
        self.refused("unknown_fixture", "status", fixture="demo", owner=owner)
        again = self.start()
        self.assertNotEqual(again["owner"], owner, "a new start gets a new owner")

    def relay_ignoring_end_of_channel(self):
        """A relay that outlives its channel, as sandbox code holding it could."""
        original = broker.RELAY
        broker.RELAY = original + "\nimport time\ntime.sleep(300)\n"
        self.addCleanup(setattr, broker, "RELAY", original)

    def end_after_test(self, pid):
        def end():
            try:
                os.kill(pid, signal.SIGKILL)
            except OSError:
                pass
        self.addCleanup(end)

    def test_stop_ends_a_sandbox_that_ignores_its_channel_and_rewrote_its_files(self):
        self.relay_ignoring_end_of_channel()
        owner = self.start()["owner"]
        fixture = self.fixtures.live["demo"]
        holder = fixture.holder_pid
        self.end_after_test(holder)
        shutil.rmtree(fixture.layout.sandbox / "project")
        self.call("stop", fixture="demo", owner=owner)
        self.assertTrue(self.await_gone(holder), "stop must end the sandbox")

    def test_the_held_pidfd_alone_ends_the_sandbox_whatever_it_rewrote(self):
        self.relay_ignoring_end_of_channel()
        self.start()
        fixture = self.fixtures.live["demo"]
        holder = fixture.holder_pid
        self.end_after_test(holder)
        shutil.rmtree(fixture.layout.sandbox / "project")
        process, fixture.process = fixture.process, None
        self.addCleanup(process.wait)
        self.addCleanup(process.kill)
        self.fixtures._kill_holder(fixture)
        self.assertTrue(self.await_gone(holder), "the pidfd kill must not depend on sandbox files")

    def test_a_sandbox_started_from_a_finished_request_thread_stays_alive(self):
        result = {}
        thread = threading.Thread(target=lambda: result.update(self.start()))
        thread.start()
        thread.join(30)
        self.assertIn("owner", result)
        time.sleep(0.3)
        status = self.call("status", fixture="demo", owner=result["owner"])
        self.assertEqual(status["runner"], "not_started")

    def test_a_killed_broker_takes_its_sandbox_along(self):
        read_end, write_end = os.pipe()
        child = os.fork()
        if child == 0:
            try:
                os.close(read_end)
                # Threads do not survive fork, so this process needs its own spawner.
                self.fixtures.spawner = broker.Spawner()
                self.start()
                pid = self.fixtures.live["demo"].holder_pid
                os.write(write_end, str(pid).encode())
                os.close(write_end)
                time.sleep(60)
            finally:
                os._exit(0)
        os.close(write_end)
        with os.fdopen(read_end) as stream:
            holder = int(stream.read())
        self.assertTrue(self.alive(holder))
        os.kill(child, signal.SIGKILL)
        os.waitpid(child, 0)
        self.assertTrue(self.await_gone(holder), "the sandbox must not outlive a killed broker")

    def test_the_viewport_is_read_only_after_the_scale_is_applied(self):
        self.harness_env["STUB_SCALE_DELAY"] = "0.4"
        started = self.start(width=800)
        self.assertEqual(started["viewport"]["scale"], 2.0)
        self.assertTrue(started["viewport"]["matches"], started["viewport"])

    def test_a_scale_that_never_applies_is_reported_within_a_bound(self):
        original = broker.VIEWPORT_APPLY_SECONDS
        broker.VIEWPORT_APPLY_SECONDS = 0.2
        self.addCleanup(setattr, broker, "VIEWPORT_APPLY_SECONDS", original)
        self.harness_env["STUB_SCALE_DELAY"] = "60"
        started = self.start(width=800)
        self.assertFalse(started["viewport"]["matches"])

    def resize_host_window(self, physical):
        state = self.base / "niri.json"
        current = json.loads(state.read_text())
        current["physical"] = physical
        state.write_text(json.dumps(current))

    def test_a_screenshot_refits_after_the_host_resizes_and_reports_a_match(self):
        owner = self.start(width=800)["owner"]
        self.resize_host_window(1200)
        shot = self.call("screenshot", fixture="demo", owner=owner, name="after")
        self.assertEqual(shot["viewport"]["width"], 800)
        self.assertTrue(shot["viewport"]["matches"], shot["viewport"])
        self.assertIn("msg output Nested scale 1.5", self.log.read_text())

    def test_input_refits_after_the_host_resizes(self):
        owner = self.start(width=800)["owner"]
        self.resize_host_window(1200)
        self.call("input", fixture="demo", owner=owner, action="key", chords=["Escape"])
        self.assertIn("msg output Nested scale 1.5", self.log.read_text())

    def test_a_screenshot_reports_a_width_it_could_not_reach(self):
        owner = self.start(width=800)["owner"]
        self.fixtures.live["demo"].width = 320
        shot = self.call("screenshot", fixture="demo", owner=owner, name="after")
        self.assertFalse(shot["viewport"]["matches"])

    def await_marker(self, path):
        deadline = time.monotonic() + 15
        while not path.exists() and time.monotonic() < deadline:
            time.sleep(0.01)
        self.assertTrue(path.exists(), path)

    def test_only_one_fixture_builds_at_a_time_and_a_pair_still_works(self):
        first = self.start("before")
        second = self.start("after")
        fixture = self.fixtures.live["before"]
        hold = fixture.layout.sandbox / "home/hold-build"
        hold.write_text("")
        self.call("run-ui", fixture="before", owner=first["owner"])
        self.await_marker(fixture.layout.cli.with_suffix(".d"))
        self.assertTrue(self.call("status", fixture="before", owner=first["owner"])["building"])
        self.refused("build_busy", "run-ui", fixture="after", owner=second["owner"])
        hold.unlink()
        self.await_marker(fixture.layout.runner_log)
        deadline = time.monotonic() + 15
        while self.call("status", fixture="before", owner=first["owner"])["building"]:
            self.assertLess(time.monotonic(), deadline)
            time.sleep(0.02)
        self.call("run-ui", fixture="after", owner=second["owner"])

    def test_a_failed_build_does_not_block_the_next_one(self):
        first = self.start("before")
        second = self.start("after")
        fixture = self.fixtures.live["before"]
        (fixture.layout.sandbox / "home/fail-build").write_text("")
        self.call("run-ui", fixture="before", owner=first["owner"])
        deadline = time.monotonic() + 15
        while self.call("status", fixture="before", owner=first["owner"])["runner"] == "running":
            self.assertLess(time.monotonic(), deadline)
            time.sleep(0.02)
        self.call("run-ui", fixture="after", owner=second["owner"])

    def plant_traps(self, fixture):
        """As sandbox code, through the relay: a tree it may then lock everyone out of."""
        layout = fixture.layout
        script = (
            f"mkdir -p {layout.sandbox}/trap/deep {layout.host.runtime_dir}/trap/deep && "
            f"ln -s {self.base} {layout.sandbox}/trap/deep/link && "
            f"chmod 000 {layout.sandbox}/trap/deep {layout.sandbox}/trap "
            f"{layout.host.runtime_dir}/trap/deep {layout.host.runtime_dir}/trap"
        )
        result = self.fixtures._in_sandbox(fixture, ["/bin/sh", "-c", script])
        self.assertEqual(result["exit_code"], 0, result)

    def test_stop_gets_past_traps_a_sandbox_left(self):
        owner = self.start()["owner"]
        fixture = self.fixtures.live["demo"]
        self.plant_traps(fixture)
        stopped = self.call("stop", fixture="demo", owner=owner)
        self.assertEqual(stopped["left_behind"], [])
        self.assertTrue(stopped["removed"])
        self.assertFalse(os.path.lexists(fixture.layout.root))
        self.assertFalse(os.path.lexists(fixture.layout.runtime))
        self.assertTrue(self.base.exists(), "the planted link must not be followed")

    def test_stop_keep_then_clear_get_past_traps(self):
        owner = self.start()["owner"]
        fixture = self.fixtures.live["demo"]
        self.plant_traps(fixture)
        stopped = self.call("stop", fixture="demo", owner=owner, keep=True)
        self.assertEqual(stopped["left_behind"], [])
        self.assertFalse(os.path.lexists(fixture.layout.runtime))
        self.assertTrue(fixture.layout.root.exists())
        self.call("clear", fixture="demo")
        self.assertFalse(os.path.lexists(fixture.layout.root))
        self.assertIn("owner", self.start())

    def test_stop_all_gets_past_traps(self):
        self.start()
        fixture = self.fixtures.live["demo"]
        self.plant_traps(fixture)
        self.assertEqual(self.fixtures.stop_all(), [])
        self.assertEqual(self.fixtures.live, {})

    def test_stop_retains_the_fixture_when_cleanup_itself_fails(self):
        owner = self.start()["owner"]
        fixture = self.fixtures.live["demo"]
        holder = fixture.holder_pid
        original = self.fixtures._discard

        def fails(_layout, *, remove):
            raise RuntimeError("cleanup failed")

        self.fixtures._discard = fails
        with self.assertRaises(RuntimeError):
            self.call("stop", fixture="demo", owner=owner)
        self.fixtures._discard = original
        self.assertIs(self.fixtures.live["demo"], fixture)
        self.assertFalse(fixture.stopped)
        self.assertIsNotNone(fixture.pidfd)
        self.assertTrue(self.await_gone(holder))
        self.call("stop", fixture="demo", owner=owner)

    def test_stop_all_continues_past_a_failure_and_reports_it(self):
        self.start("a")
        self.start("b")
        holders = [fixture.holder_pid for fixture in self.fixtures.live.values()]
        original = self.fixtures.stop
        visited = []

        def fails_first(fixture, request):
            visited.append(fixture.layout.name)
            if len(visited) == 1:
                raise RuntimeError("cleanup failed")
            return original(fixture, request)

        self.fixtures.stop = fails_first
        problems = self.fixtures.stop_all()
        self.fixtures.stop = original
        self.assertEqual(len(visited), 2)
        self.assertEqual(len(problems), 1)
        self.assertIn("cleanup failed", problems[0])
        for holder in holders:
            self.assertTrue(self.await_gone(holder))

    def test_startup_recovery_continues_past_a_failure_and_reports_it(self):
        private = self.host.runtime_dir / broker.PRIVATE_DIR
        (private / "aa").mkdir(parents=True)
        (private / "bb").mkdir()
        original = self.fixtures._discard
        visited = []

        def fails_first(layout, *, remove):
            visited.append(layout.name)
            if len(visited) == 1:
                raise RuntimeError("recovery failed")
            return original(layout, remove=remove)

        self.fixtures._discard = fails_first
        recovered, problems = self.fixtures.recover_stale()
        self.fixtures._discard = original
        self.assertEqual(len(visited), 2)
        self.assertEqual(len(recovered), 1)
        self.assertIn("recovery failed", problems[0])

    def test_clear_holds_its_name_until_it_has_finished(self):
        entered, proceed = threading.Event(), threading.Event()
        original = self.fixtures._discard

        def waits(layout, *, remove):
            entered.set()
            proceed.wait(10)
            return original(layout, remove=remove)

        self.fixtures._discard = waits
        thread = threading.Thread(target=lambda: self.call("clear", fixture="demo"))
        thread.start()
        self.assertTrue(entered.wait(10))
        try:
            with self.assertRaises(broker.Refusal) as start_refused:
                self.fixtures.start({"op": "start", "fixture": "demo", "checkout": self.checkout, "width": 800})
            with self.assertRaises(broker.Refusal) as clear_refused:
                self.call("clear", fixture="demo")
        finally:
            proceed.set()
            thread.join(15)
            self.fixtures._discard = original
        self.assertEqual(start_refused.exception.code, "fixture_exists")
        self.assertEqual(clear_refused.exception.code, "fixture_exists")
        self.assertIn("owner", self.start())

    def test_the_sandbox_has_its_own_session_and_no_terminal(self):
        self.start()
        holder = self.fixtures.live["demo"].holder_pid
        fields = Path(f"/proc/{holder}/stat").read_text().rpartition(")")[2].split()
        self.assertNotEqual(int(fields[3]), os.getsid(0), "it must not share this process's session")
        self.assertEqual(int(fields[4]), 0, "it must have no controlling terminal")

    def test_at_most_two_fixtures_run_at_once(self):
        self.start("a")
        self.start("b")
        self.refused("too_many_fixtures", "start", fixture="c", checkout="wt")

    def test_a_failed_start_frees_its_name_without_leaving_a_sandbox(self):
        self.harness_env["STUB_START_FAILS"] = "1"
        self.refused("harness_failed", "start", fixture="demo", checkout="wt")
        self.assertEqual(self.fixtures.live, {})
        self.assertFalse((self.repo / ".worktrees/native-fixture/demo").exists())
        del self.harness_env["STUB_START_FAILS"]
        self.assertIn("owner", self.start())

    def test_a_sandbox_that_never_greets_is_killed_when_start_fails(self):
        relay = broker.RELAY
        # Never touches its channel, so closing that channel cannot end it.
        broker.RELAY = "import time\nopen('.running', 'w').close()\ntime.sleep(300)\n"
        self.addCleanup(setattr, broker, "RELAY", relay)
        seen = {}
        original = broker.RelayChannel.await_ready

        def refuse_after_noting(channel, timeout):
            fixture = self.fixtures.live["demo"]
            seen["holder"] = fixture.holder_pid
            self.end_after_test(seen["holder"])
            # Past bwrap's setup, which removing the fixture's directories could also end.
            marker = fixture.layout.sandbox / "project/.running"
            deadline = time.monotonic() + 15
            while not marker.exists() and time.monotonic() < deadline:
                time.sleep(0.01)
            self.assertTrue(marker.exists())
            raise broker.Refusal("sandbox_failed", "no greeting")

        broker.RelayChannel.await_ready = refuse_after_noting
        self.addCleanup(setattr, broker.RelayChannel, "await_ready", original)
        self.refused("sandbox_failed", "start", fixture="demo", checkout="wt")
        self.assertTrue(self.await_gone(seen["holder"]))
        self.assertEqual(self.fixtures.live, {})

    def test_a_request_that_waited_through_a_stop_does_not_reach_the_next_fixture(self):
        owner = self.start()["owner"]
        old = self.fixtures.live["demo"]
        old.lock.acquire()
        outcome = {}

        def waiter():
            try:
                outcome["result"] = self.call("status", fixture="demo", owner=owner)
            except broker.Refusal as refusal:
                outcome["code"] = refusal.code

        thread = threading.Thread(target=waiter)
        thread.start()
        time.sleep(0.2)
        self.fixtures.stop(old, {"keep": False})
        old.lock.release()
        thread.join(10)
        self.assertEqual(outcome.get("code"), "unknown_fixture", outcome)

    def test_stop_does_not_wait_for_a_busy_fixture_to_end_its_sandbox(self):
        owner = self.start()["owner"]
        fixture = self.fixtures.live["demo"]
        holder = fixture.holder_pid
        fixture.lock.acquire()
        original = broker.LOCK_SECONDS
        broker.LOCK_SECONDS = 0.1
        self.addCleanup(setattr, broker, "LOCK_SECONDS", original)
        self.refused("busy", "stop", fixture="demo", owner=owner)
        fixture.lock.release()
        self.assertTrue(self.await_gone(holder))

    def test_stop_all_does_not_wait_forever_on_a_held_lock(self):
        self.start()
        fixture = self.fixtures.live["demo"]
        holder = fixture.holder_pid
        fixture.lock.acquire()
        self.addCleanup(fixture.lock.release)
        original = broker.STOP_ALL_LOCK_SECONDS
        broker.STOP_ALL_LOCK_SECONDS = 0.2
        self.addCleanup(setattr, broker, "STOP_ALL_LOCK_SECONDS", original)
        thread = threading.Thread(target=self.fixtures.stop_all, daemon=True)
        thread.start()
        thread.join(8)
        self.assertFalse(thread.is_alive(), "stop_all must not wait forever on a held lock")
        self.assertTrue(self.await_gone(holder))

    def test_a_binary_from_another_checkout_does_not_count_as_built(self):
        owner = self.start()["owner"]
        fixture = self.fixtures.live["demo"]
        cli = fixture.layout.cli
        cli.parent.mkdir(parents=True)
        cli.write_text("#!/bin/sh\n")
        cli.with_suffix(".d").write_text(f"{cli}: {self.repo}/.worktrees/other/src/main.rs\n")
        fixture.runner_started = True
        self.assertFalse(self.call("status", fixture="demo", owner=owner)["cli_built"])
        self.refused("cli_not_built", "cli", fixture="demo", owner=owner, args=["version"])

    def test_a_binary_from_this_checkout_needs_this_fixtures_runner(self):
        owner = self.start()["owner"]
        fixture = self.fixtures.live["demo"]
        cli = fixture.layout.cli
        cli.parent.mkdir(parents=True)
        cli.with_suffix(".d").write_text(f"{cli}: {self.checkout}/src/main.rs\n")
        self.assertFalse(self.call("status", fixture="demo", owner=owner)["cli_built"])

    def test_clear_recovers_a_left_behind_name_but_not_a_live_one(self):
        owner = self.start()["owner"]
        self.refused("fixture_exists", "clear", fixture="demo")
        self.call("stop", fixture="demo", owner=owner, keep=True)
        self.refused("fixture_exists", "start", fixture="demo", checkout="wt")
        self.call("clear", fixture="demo")
        self.assertIn("session stop demo", self.log.read_text())
        self.assertIn("owner", self.start())

    def test_clear_leaves_a_symlinked_parent_alone(self):
        victim = self.base / "victim"
        (victim / "demo").mkdir(parents=True)
        (victim / "demo/keep").write_text("keep")
        (self.repo / ".worktrees/native-fixture").symlink_to(victim)
        self.refused("not_removed", "clear", fixture="demo")
        self.assertEqual((victim / "demo/keep").read_text(), "keep")

    def test_startup_recovers_sessions_left_by_a_killed_broker(self):
        stale = self.host.runtime_dir / broker.PRIVATE_DIR / "old"
        (stale / "state").mkdir(parents=True)
        (self.host.runtime_dir / "nf-old").mkdir()
        self.assertEqual(self.fixtures.recover_stale(), (["old"], []))
        self.assertIn("session stop old", self.log.read_text())
        self.assertFalse(stale.exists())
        self.assertFalse((self.host.runtime_dir / "nf-old").exists())

    def test_relay_runs_commands_in_one_namespace_with_limits_and_bounded_output(self):
        owner = self.start()["owner"]
        fixture = self.fixtures.live["demo"]
        self.addCleanup(lambda: None)
        script = (
            "import os, resource;"
            f"print(os.getpid(), {AUTHORITY!r} in os.environ, resource.getrlimit(resource.RLIMIT_NPROC)[0],"
            " resource.getrlimit(resource.RLIMIT_CORE)[0], flush=True)"
        )
        result = self.fixtures._in_sandbox(fixture, ["/usr/bin/python3", "-I", "-c", script])
        self.assertEqual(result["exit_code"], 0, result)
        pid, token, nproc, core = result["stdout"].split()
        self.assertLess(int(pid), 100, "the command must run in the sandbox's PID namespace")
        self.assertEqual(token == "True", AUTHORITY in os.environ)
        self.assertEqual(int(nproc), 8192)
        self.assertEqual(int(core), 0)

        noisy = fixture.channel.call(
            {"argv": ["/usr/bin/python3", "-I", "-c", "import sys;sys.stdout.write('x'*500000)"],
             "cwd": str(fixture.layout.sandbox / "project"), "timeout": 20},
            25,
        )
        self.assertEqual(len(noisy["stdout"]), broker.MAX_OUTPUT_BYTES, "the relay itself must send only a tail")

        missing = self.fixtures._in_sandbox(fixture, ["/no/such/program"])
        self.assertEqual(missing["exit_code"], 127)
        with self.assertRaises(broker.Refusal) as caught:
            self.fixtures._in_sandbox(fixture, ["/usr/bin/sleep", "30"], timeout=0.2)
        self.assertEqual(caught.exception.code, "timeout")
        self.assertEqual(self.call("status", fixture="demo", owner=owner)["runner"], "not_started")


STATUS = b'{"op":"status","fixture":"a","owner":"' + OWNER.encode() + b'"}\n'


class OwnershipContracts(TemporaryRepository):
    """Inert/mock ownership oracles; no broker guard, native child or sandbox.

    Synthetic descriptor numbers are used only while all corresponding OS
    operations are mocked. No inherited environment or authority is changed.
    """

    def setUp(self):
        super().setUp()
        self.host = broker.Host(
            repo=self.repo, home=self.base / "home", runtime_dir=self.base / "runtime",
            cargo_home=self.base / "cargo", rustup_home=self.base / "rustup",
            mise_data=self.base / "mise", harness="synthetic-harness", niri="synthetic-niri",
        )
        self.manager = object.__new__(broker.Fixtures)
        self.manager.host = self.host
        self.manager.harness_env = {"SYNTHETIC_FIXTURE": "mock only; no child executes"}
        self.manager.lock = threading.Lock()
        self.manager.build_lock = threading.Lock()
        self.manager.closing = False
        self.manager.clearing = set()
        self.manager.host_operations = []
        self.manager.spawner = mock.Mock()
        self.manager.spawner.close.return_value = []
        self.fixture = self.owned_fixture("demo", 44)
        self.manager.live = {"demo": self.fixture}

    def owned_fixture(self, name, descriptor):
        layout = broker.Layout(self.host, name, self.checkout, self.base / "zig")
        fixture = broker.Fixture(layout, OWNER)
        fixture.process = mock.Mock(pid=700 + descriptor, returncode=None)
        fixture.process.wait.return_value = 0
        fixture.pidfd = descriptor
        fixture.channel = mock.Mock()
        fixture.host_session_attempted = True
        return fixture

    def stop_seams(self, ready=True):
        stack = ExitStack()
        stack.enter_context(mock.patch.object(broker.signal, "pidfd_send_signal"))
        stack.enter_context(mock.patch.object(broker.select, "select", side_effect=lambda read, *_: (read if ready else [], [], [])))
        self.close_fd = stack.enter_context(mock.patch.object(broker.os, "close"))
        self.host_stop = stack.enter_context(mock.patch.object(
            self.manager, "harness", return_value=subprocess.CompletedProcess([], 0, b"", b"")))
        self.remove = stack.enter_context(mock.patch.object(broker, "remove_beneath", return_value=True))
        return stack

    def test_original_wait_timeout_retains_name_pidfd_channel_and_controls(self):
        original = self.fixture.process
        channel = self.fixture.channel
        original.wait.side_effect = subprocess.TimeoutExpired("synthetic-owned-child", 5)
        with self.stop_seams():
            with self.assertRaises(broker.Refusal) as caught:
                self.manager.stop(self.fixture, {"keep": False})
            self.assertEqual(caught.exception.code, "cleanup_incomplete")
            self.assertIs(self.manager.live["demo"], self.fixture)
            self.assertIs(self.fixture.process, original)
            self.assertEqual(self.fixture.pidfd, 44)
            self.assertFalse(self.fixture.stopped)
            channel.close.assert_not_called()
            self.close_fd.assert_not_called()
            self.host_stop.assert_not_called()
            self.remove.assert_not_called()

    def test_signal_attempt_does_not_replace_positive_held_exit(self):
        with self.stop_seams(ready=False), mock.patch.object(
                broker.signal, "pidfd_send_signal", side_effect=OSError("synthetic signal refusal")):
            with self.assertRaises(broker.Refusal):
                self.manager.stop(self.fixture, {})
            self.fixture.process.wait.assert_called_once()
            self.assertFalse(self.fixture.settlement["held_pidfd_exited"])
            self.assertIs(self.manager.live["demo"], self.fixture)
            self.host_stop.assert_not_called()
            self.close_fd.assert_not_called()

    def test_positive_original_wait_and_host_stop_precede_release_and_removal(self):
        events = []
        self.fixture.process.wait.side_effect = lambda **_: events.append("original-wait") or 0
        channel = self.fixture.channel
        with self.stop_seams():
            self.host_stop.side_effect = lambda *_a, **_k: events.append("host-stop") or subprocess.CompletedProcess([], 0, b"", b"")
            self.close_fd.side_effect = lambda _fd: events.append("close")
            self.remove.side_effect = lambda *_a: events.append("remove") or True
            result = self.manager.stop(self.fixture, {"keep": False})
            self.assertEqual(result["left_behind"], [])
            self.assertTrue(result["removed"])
            self.assertLess(events.index("original-wait"), events.index("host-stop"))
            self.assertLess(events.index("host-stop"), events.index("close"))
            self.assertLess(events.index("close"), events.index("remove"))
            self.assertEqual(self.manager.live, {})
            self.assertTrue(self.fixture.released)
            channel.close.assert_called_once()

    def test_failed_signal_stays_failure_after_positive_settlement_and_cleanup(self):
        with self.stop_seams(), mock.patch.object(
                broker.select, "select", side_effect=[([], [], []), ([44], [], [])]), mock.patch.object(
                broker.signal, "pidfd_send_signal", side_effect=OSError("synthetic signal failure")):
            with self.assertRaises(broker.Refusal) as caught:
                self.manager.stop(self.fixture, {})
            self.assertIn("synthetic signal failure", caught.exception.detail)
            self.assertTrue(self.fixture.released)
            self.assertTrue(self.fixture.stopped)
            self.assertEqual(self.manager.live, {})
            self.remove.assert_called()

    def test_already_settled_original_does_not_signal_an_exited_pidfd_again(self):
        self.fixture.process.returncode = 0
        with self.stop_seams(), mock.patch.object(broker.signal, "pidfd_send_signal") as send:
            self.manager.stop(self.fixture, {})
            send.assert_not_called()
            self.fixture.process.kill.assert_not_called()
            self.fixture.process.wait.assert_called_once()
            self.assertEqual(self.manager.live, {})

    def test_early_stop_wait_failure_is_not_overwritten_by_later_settlement(self):
        self.fixture.process.wait.side_effect = [subprocess.TimeoutExpired("synthetic first wait", 5), 0]
        with self.stop_seams():
            with self.assertRaises(broker.Refusal) as caught:
                self.manager.handle({"op": "stop", "fixture": "demo", "owner": OWNER})
            self.assertIn("synthetic first wait", caught.exception.detail)
            self.assertEqual(self.fixture.process.wait.call_count, 2)
            self.assertTrue(self.fixture.released)
            self.assertEqual(self.manager.live, {})

    def test_failed_or_timed_out_host_stop_retains_private_controls(self):
        for failure in (subprocess.CompletedProcess([], 9, b"", b"synthetic failed stop"),
                        subprocess.TimeoutExpired("synthetic host stop", 30)):
            with self.subTest(failure=type(failure).__name__):
                self.fixture = self.owned_fixture("demo", 44)
                self.manager.live = {"demo": self.fixture}
                with self.stop_seams():
                    if isinstance(failure, BaseException):
                        self.host_stop.side_effect = failure
                    else:
                        self.host_stop.return_value = failure
                    with self.assertRaises((broker.Refusal, subprocess.TimeoutExpired)):
                        self.manager.stop(self.fixture, {})
                    self.assertIs(self.manager.live["demo"], self.fixture)
                    self.assertFalse(self.fixture.host_session_stopped)
                    self.assertFalse(self.fixture.stopped)
                    self.close_fd.assert_not_called()
                    self.remove.assert_not_called()

    def test_cleanup_pass_continues_past_an_unsettled_original(self):
        other = self.owned_fixture("other", 45)
        self.manager.live["other"] = other
        self.fixture.process.wait.side_effect = subprocess.TimeoutExpired("synthetic first", 5)
        with self.stop_seams():
            problems = self.manager.stop_all()
            self.assertTrue(any("demo" in problem for problem in problems))
            self.assertIs(self.manager.live["demo"], self.fixture)
            self.assertNotIn("other", self.manager.live)
            self.assertTrue(other.stopped)
            self.manager.spawner.close.assert_not_called()

    def test_shutdown_fences_a_start_that_was_already_accepted(self):
        self.manager.live = {}
        self.manager.layout_for = mock.Mock(return_value=self.fixture.layout)
        self.manager._host_focus = mock.Mock(return_value=(None, set()))
        seen = []

        def held_create(fixture, _width):
            seen.append(fixture)
            self.manager.begin_shutdown()
            problems = self.manager.stop_all()
            self.assertTrue(any("busy" in problem for problem in problems))
            self.assertIs(self.manager.live["demo"], fixture)
            self.manager._spawn_holder(fixture, "synthetic-display")

        self.manager._create = held_create
        with mock.patch.object(broker, "STOP_ALL_LOCK_SECONDS", 0), self.stop_seams(), mock.patch.object(
                broker, "hold_binds") as binds, mock.patch.object(broker, "stopping"):
            with self.assertRaises(broker.Refusal) as caught:
                self.manager.start({"fixture": "demo", "checkout": self.checkout, "width": 960})
            self.assertEqual(caught.exception.code, "shutting_down")
            self.assertEqual(len(seen), 1)
            binds.assert_not_called()
            self.manager.spawner.run.assert_not_called()
            self.assertEqual(self.manager.live, {})
            self.assertTrue(seen[0].released)
            with self.assertRaises(broker.Refusal):
                self.manager.start({"fixture": "later", "checkout": self.checkout, "width": 960})
            self.manager.layout_for.assert_called_once()

    def test_original_request_join_retries_interruptions_without_losing_handle(self):
        server = object.__new__(broker.Server)
        server.requests_lock = threading.Lock()
        server.accepting = True
        server.listen_close_attempted = False
        server.socket_close_uncertain = False
        original = mock.Mock()
        original.join.side_effect = [KeyboardInterrupt(), OSError("synthetic interrupted join"), None]
        record = {"thread": original, "joined": False, "start_error": None, "close_error": None}
        server.request_threads = [record]
        with mock.patch.object(broker.socketserver.UnixStreamServer, "server_close") as close, mock.patch.object(
                broker.time, "sleep"), mock.patch.object(broker, "stopping") as diagnostic:
            problems = server.server_close()
            self.assertEqual(original.join.call_count, 3)
            self.assertEqual(len(problems), 1)
            self.assertTrue(record["joined"])
            self.assertTrue(server.ownership_settled())
            diagnostic.assert_called_once()
            self.assertEqual(server.server_close(), [])
            close.assert_called_once()

    def test_request_start_uncertainty_retains_original_thread_and_request(self):
        server = object.__new__(broker.Server)
        server.requests_lock = threading.Lock()
        server.accepting = True
        server.request_threads = []
        server.slots = mock.Mock()
        server.shutdown_request = mock.Mock()
        server.listen_close_attempted = False
        server.socket_close_uncertain = False
        original, request = mock.Mock(), object()
        original.start.side_effect = KeyboardInterrupt()
        with mock.patch.object(broker.threading, "Thread", return_value=original), mock.patch.object(
                broker, "stopping"), mock.patch.object(broker.socketserver.UnixStreamServer, "server_close"):
            server.process_request(request, "synthetic-client")
            record, = server.request_threads
            self.assertIs(record["thread"], original)
            self.assertIs(record["request"], request)
            self.assertFalse(record["joined"])
            server.shutdown_request.assert_not_called()
            server.slots.release.assert_not_called()
            self.assertTrue(server.server_close())
            original.join.assert_called_once_with()
            self.assertTrue(record["joined"])
            self.assertTrue(server.ownership_settled())

    def test_closed_request_admission_closes_only_the_accepted_socket(self):
        server = object.__new__(broker.Server)
        server.requests_lock = threading.Lock()
        server.accepting = False
        server.request_threads = []
        server.slots = mock.Mock()
        server.shutdown_request = mock.Mock()
        request = object()
        with mock.patch.object(broker.threading, "Thread") as worker:
            self.assertFalse(server.verify_request(request, "synthetic-client"))
            server.slots.acquire.assert_not_called()
            # process_request handles a request accepted before admission closed.
            server.process_request(request, "synthetic-client")
            worker.assert_not_called()
            server.shutdown_request.assert_called_once_with(request)
            server.slots.release.assert_called_once_with()
            self.assertTrue(server.request_threads[0]["joined"])

    def test_shutdown_keeps_first_failure_after_later_positive_drain(self):
        fixtures = mock.Mock()
        fixtures.lock = threading.Lock()
        fixtures.live, fixtures.clearing, fixtures.host_operations = {}, set(), []
        fixtures.stop_all.side_effect = [["synthetic original cleanup failure"], []]
        server = mock.Mock()
        server.server_close.side_effect = [KeyboardInterrupt(), []]
        server.ownership_settled.return_value = True
        with mock.patch.object(broker.time, "sleep"), mock.patch.object(broker, "stopping"):
            self.assertTrue(broker.shutdown(server, fixtures))
        fixtures.begin_shutdown.assert_called_once()
        self.assertEqual(server.server_close.call_count, 2)
        self.assertEqual(fixtures.stop_all.call_count, 2)

    def test_unknown_runner_refuses_another_build_until_positive_completion(self):
        self.fixture.runner_started = True
        other = self.owned_fixture("other", 45)
        self.manager.live["other"] = other
        with mock.patch.object(broker, "read_regular", return_value=None) as log, mock.patch.object(
                self.manager, "_holder_alive", return_value=True), mock.patch.object(
                self.manager, "_runner_state", side_effect=broker.Refusal("timeout", "synthetic unreadable runner")) as state, mock.patch.object(
                self.manager, "_start_runner", return_value={"state": "synthetic-start"}) as start:
            with self.assertRaises(broker.Refusal) as caught:
                self.manager.run_ui(other, {})
            self.assertEqual(caught.exception.code, "build_busy")
            start.assert_not_called()
            state.side_effect = None
            state.return_value = "not_started"
            with self.assertRaises(broker.Refusal):
                self.manager.run_ui(other, {})
            state.return_value = "exited_1"
            self.assertEqual(self.manager.run_ui(other, {}), {"state": "synthetic-start"})
            log.return_value = broker.BUILD_FINISHED_MARKER
            self.manager.run_ui(other, {})
            self.assertEqual(start.call_count, 2)

    def test_unacknowledged_runner_launch_blocks_other_build_without_cli_credit(self):
        failures = (broker.Refusal("timeout", "synthetic lost launch reply"),
                    subprocess.TimeoutExpired("synthetic launch reply", 10),
                    {}, {"state": "not_started"})
        for failure in failures:
            with self.subTest(failure=type(failure).__name__, reply=repr(failure)):
                first = self.owned_fixture("demo", 44)
                other = self.owned_fixture("other", 45)
                self.manager.live = {"demo": first, "other": other}

                def original_reply(request, _timeout):
                    if request == {"poll": "runner"}:
                        return {"state": "not_started"}
                    self.assertEqual(request["background"], "runner")
                    self.assertTrue(first.runner_attempted)
                    self.assertFalse(first.runner_started)
                    if isinstance(failure, BaseException):
                        raise failure
                    return failure

                first.channel.call.side_effect = original_reply
                with self.assertRaises((broker.Refusal, subprocess.TimeoutExpired)):
                    self.manager.run_ui(first, {})
                self.assertTrue(first.runner_attempted)
                self.assertFalse(first.runner_started)
                self.assertEqual(first.channel.call.call_count, 2)
                with mock.patch.object(broker, "read_regular", return_value=None), mock.patch.object(
                        self.manager, "_holder_alive", return_value=True), mock.patch.object(
                        self.manager, "_runner_state", side_effect=broker.Refusal("timeout", "synthetic unknown runner")):
                    with self.assertRaises(broker.Refusal) as caught:
                        self.manager.run_ui(other, {})
                    self.assertEqual(caught.exception.code, "build_busy")
                    other.channel.call.assert_not_called()
                    self.assertEqual(first.channel.call.call_count, 2)
                matching_dep_info = f"{first.layout.cli}: {first.layout.checkout}/src/main.rs\n".encode()
                with mock.patch.object(broker, "read_regular", return_value=matching_dep_info), mock.patch.object(
                        self.manager, "_in_sandbox") as command:
                    self.assertFalse(self.manager._cli_built(first))
                    with self.assertRaises(broker.Refusal) as caught:
                        self.manager.cli(first, {"args": ["version"]})
                    self.assertEqual(caught.exception.code, "cli_not_built")
                    command.assert_not_called()
                with mock.patch.object(broker, "read_regular", return_value=None), mock.patch.object(
                        self.manager, "_holder_alive", return_value=True), mock.patch.object(
                        self.manager, "_runner_state", return_value="exited_1"), mock.patch.object(
                        self.manager, "_start_runner", return_value={"state": "synthetic-start"}) as start:
                    self.assertEqual(self.manager.run_ui(other, {}), {"state": "synthetic-start"})
                    start.assert_called_once_with(other)
                self.assertFalse(first.runner_started)

    def test_acknowledged_runner_launch_retains_successful_cli_receipt_control(self):
        self.fixture.channel.call.side_effect = [{"state": "not_started"}, {"state": "running"}]
        result = self.manager.run_ui(self.fixture, {})
        self.assertEqual(result["state"], "building_then_launching")
        self.assertTrue(self.fixture.runner_attempted)
        self.assertTrue(self.fixture.runner_started)
        matching_dep_info = f"{self.fixture.layout.cli}: {self.fixture.layout.checkout}/src/main.rs\n".encode()
        with mock.patch.object(broker, "read_regular", return_value=matching_dep_info):
            self.assertTrue(self.manager._cli_built(self.fixture))

    def test_retry_uses_current_log_for_build_admission_and_status(self):
        first = self.fixture
        other = self.owned_fixture("other", 45)
        self.manager.live["other"] = other
        logs, state = {}, {"poll": "not_started"}

        def reply(request, _timeout):
            if request == {"poll": "runner"}:
                return {"state": state["poll"]}
            self.assertEqual(request["background"], "runner")
            self.assertEqual(request["log"], str(first.runner_log))
            self.assertFalse(first.runner_started)
            self.assertNotIn(request["log"], logs)
            logs[request["log"]] = b"synthetic current cold build\n"
            state["poll"] = "running"
            return {"state": "running"}

        def read(root, parts, name, _limit):
            return logs.get(str(Path(root).joinpath(*parts, name)))

        first.channel.call.side_effect = reply
        with mock.patch.object(broker, "read_regular", side_effect=read) as reads, mock.patch.object(
                self.manager, "_holder_alive", return_value=True):
            initial = self.manager.run_ui(first, {})
            previous = first.runner_log
            self.assertEqual(previous, first.layout.runner_log)
            self.assertEqual(initial["runner_log"], str(previous))
            logs[str(previous)] = broker.BUILD_FINISHED_MARKER
            state["poll"] = "exited_0"
            current = self.manager.run_ui(first, {})
            self.assertEqual(first.runner_attempt, 2)
            self.assertEqual(first.runner_log, first.layout.sandbox / "runner-2.log")
            self.assertEqual(current["runner_log"], str(first.runner_log))
            self.assertEqual(logs[str(previous)], broker.BUILD_FINISHED_MARKER)
            reads.reset_mock()
            status = self.manager.status(first, {})
            self.assertTrue(status["building"])
            self.assertEqual(status["runner_log"], current["runner_log"])
            with self.assertRaises(broker.Refusal) as caught:
                self.manager.run_ui(other, {})
            self.assertEqual(caught.exception.code, "build_busy")
            other.channel.call.assert_not_called()
            self.assertTrue(self.manager._is_building(first))
            logs[str(first.runner_log)] = broker.BUILD_FINISHED_MARKER
            self.assertFalse(self.manager.status(first, {})["building"])
            self.assertFalse(self.manager._is_building(first))
            with mock.patch.object(self.manager, "_start_runner", return_value={"state": "synthetic-start"}) as start:
                self.assertEqual(self.manager.run_ui(other, {}), {"state": "synthetic-start"})
                start.assert_called_once_with(other)
            read_names = [call.args[2] for call in reads.call_args_list]
            self.assertIn("runner-2.log", read_names)
            self.assertNotIn("runner.log", read_names)
            self.assertEqual(logs[str(previous)], broker.BUILD_FINISHED_MARKER)

    def test_retry_without_ack_keeps_current_attempt_busy_and_drops_old_cli_credit(self):
        for failure in (broker.Refusal("timeout", "synthetic retry ACK lost"), {"state": "not_started"}):
            with self.subTest(failure=type(failure).__name__):
                first = self.owned_fixture("demo", 44)
                other = self.owned_fixture("other", 45)
                self.manager.live = {"demo": first, "other": other}
                logs, state = {}, {"poll": "not_started", "launches": 0}
                logs[str(first.layout.cli.with_suffix(".d"))] = f"{first.layout.cli}: {first.layout.checkout}/src/main.rs\n".encode()

                def reply(request, _timeout):
                    if request == {"poll": "runner"}:
                        if state["poll"] == "unreadable":
                            raise broker.Refusal("timeout", "synthetic unreadable retry")
                        return {"state": state["poll"]}
                    self.assertEqual(request["log"], str(first.runner_log))
                    self.assertFalse(first.runner_started)
                    self.assertTrue(first.runner_attempted)
                    self.assertNotIn(request["log"], logs)
                    logs[request["log"]] = b"synthetic uncompleted retry\n"
                    state["launches"] += 1
                    if state["launches"] == 1:
                        state["poll"] = "running"
                        return {"state": "running"}
                    if isinstance(failure, BaseException):
                        state["poll"] = "unreadable"
                        raise failure
                    state["poll"] = "not_started"
                    return failure

                def read(root, parts, name, _limit):
                    return logs.get(str(Path(root).joinpath(*parts, name)))

                first.channel.call.side_effect = reply
                with mock.patch.object(broker, "read_regular", side_effect=read), mock.patch.object(
                        self.manager, "_holder_alive", return_value=True):
                    self.manager.run_ui(first, {})
                    self.assertTrue(self.manager._cli_built(first))
                    previous = first.runner_log
                    logs[str(previous)] = broker.BUILD_FINISHED_MARKER
                    state["poll"] = "exited_0"
                    with self.assertRaises(broker.Refusal):
                        self.manager.run_ui(first, {})
                    self.assertEqual(first.runner_attempt, 2)
                    self.assertTrue(first.runner_attempted)
                    self.assertFalse(first.runner_started)
                    self.assertFalse(self.manager._cli_built(first))
                    self.assertTrue(self.manager._building_now(first))
                    if isinstance(failure, BaseException):
                        with self.assertRaises(broker.Refusal):
                            self.manager.status(first, {})
                    else:
                        status = self.manager.status(first, {})
                        self.assertTrue(status["building"])
                        self.assertFalse(status["cli_built"])
                        self.assertEqual(status["runner_log"], str(first.runner_log))
                    with self.assertRaises(broker.Refusal) as caught:
                        self.manager.run_ui(other, {})
                    self.assertEqual(caught.exception.code, "build_busy")
                    other.channel.call.assert_not_called()
                    self.assertEqual(logs[str(previous)], broker.BUILD_FINISHED_MARKER)

    def test_fixed_relay_refuses_existing_log_before_child_dispatch(self):
        # Future execution extracts only the fixed run definition. No RELAY
        # imports/socket loop execute; all IO/process globals are explicit mocks.
        functions = [node for node in ast.parse(broker.RELAY).body
                     if isinstance(node, ast.FunctionDef) and node.name == "run"]
        self.assertEqual(len(functions), 1)
        process = mock.Mock()
        process.TimeoutExpired = subprocess.TimeoutExpired
        process.DEVNULL, process.STDOUT = subprocess.DEVNULL, subprocess.STDOUT
        opening = mock.Mock(side_effect=FileExistsError("synthetic existing private log"))
        previous, child = object(), object()
        children = {"runner": previous}
        namespace = {"__builtins__": {"dict": dict, "str": str, "OSError": OSError},
                     "children": children, "subprocess": process, "tempfile": mock.Mock(), "open": opening}
        exec(compile(ast.Module(body=functions, type_ignores=[]), "<fixed-relay-run-mock>", "exec"), namespace)
        request = {"argv": ["/synthetic/runner"], "cwd": "/synthetic/project",
                   "env": {"SYNTHETIC_FIXTURE": "mock only"}, "background": "runner", "log": "/synthetic/runner-2.log"}
        refused = namespace["run"](request)
        self.assertEqual(refused["exit_code"], 127)
        self.assertIn("synthetic existing private log", refused["stderr"])
        opening.assert_called_once_with(request["log"], "xb")
        process.Popen.assert_not_called()
        process.run.assert_not_called()
        self.assertIs(children["runner"], previous)
        opening.side_effect = None
        context = mock.MagicMock()
        log = context.__enter__.return_value
        opening.return_value = context
        process.Popen.return_value = child
        self.assertEqual(namespace["run"](request), {"state": "running"})
        process.Popen.assert_called_once_with(stdout=log, stderr=subprocess.STDOUT, args=request["argv"],
                                             cwd=request["cwd"], env=request["env"], stdin=subprocess.DEVNULL,
                                             start_new_session=True)
        self.assertIs(children["runner"], child)

    def test_partial_holder_acquisitions_close_every_prior_original_once(self):
        for stage in ("pipe", "socketpair", "RelayChannel", "Popen", "pidfd"):
            with self.subTest(stage=stage):
                fixture = broker.Fixture(self.fixture.layout, OWNER)
                binds, ours, theirs, channel, child = (mock.Mock() for _ in range(5))
                binds.fds.return_value = (100, 101, 102, 103, 104)
                theirs.fileno.return_value = 301
                self.manager.spawner.run.side_effect = lambda job: job()
                failure = OSError("synthetic " + stage)
                with mock.patch.object(broker, "hold_binds", return_value=binds), mock.patch.object(
                        broker.os, "pipe", side_effect=failure if stage == "pipe" else None, return_value=(200, 201)), mock.patch.object(
                        broker.socket, "socketpair", side_effect=failure if stage == "socketpair" else None, return_value=(ours, theirs)), mock.patch.object(
                        broker, "RelayChannel", side_effect=failure if stage == "RelayChannel" else None, return_value=channel), mock.patch.object(
                        broker, "holder_argv", return_value=["synthetic-no-execution"]), mock.patch.object(
                        broker.subprocess, "Popen", side_effect=failure if stage == "Popen" else None, return_value=child), mock.patch.object(
                        broker.os, "pidfd_open", side_effect=failure), mock.patch.object(broker.os, "close") as close:
                    with self.assertRaises(OSError) as caught:
                        self.manager._spawn_holder(fixture, "synthetic-display")
                    self.assertIs(caught.exception, failure)
                    binds.close.assert_called_once()
                    self.assertEqual(close.call_args_list, [] if stage == "pipe" else [mock.call(201), mock.call(200)] if stage in ("Popen", "pidfd") else [mock.call(200), mock.call(201)])
                    if stage in ("Popen", "pidfd"):
                        theirs.close.assert_called_once()
                        channel.close.assert_not_called()
                    if stage == "RelayChannel":
                        ours.close.assert_called_once()
                        theirs.close.assert_called_once()
                    if stage == "pidfd":
                        self.assertIs(fixture.process, child)
                        child.wait.assert_not_called()
                    self.assertEqual(fixture.spawn_resources, {})

    def test_partial_bind_failure_attempts_all_original_closes_and_keeps_first_error(self):
        original = OSError("synthetic last bind acquisition")
        with mock.patch.object(broker, "hold_directory", side_effect=[100, 101, 102, 103]), mock.patch.object(
                broker, "hold_socket", side_effect=original), mock.patch.object(
                broker.os, "close", side_effect=[OSError("synthetic first close"), None, None, None]) as close:
            with self.assertRaises(OSError) as caught:
                broker.hold_binds(self.fixture.layout, self.host.runtime_dir, "synthetic-wayland")
        self.assertIs(caught.exception, original)
        self.assertEqual(close.call_args_list, [mock.call(value) for value in range(100, 104)])
        self.assertEqual(len(original.broker_close_errors), 1)

    def test_pidfd_open_failure_keeps_created_child_until_original_wait_settles(self):
        self.manager.live = {}
        self.manager.layout_for = mock.Mock(return_value=self.fixture.layout)
        self.manager._create = lambda fixture, _width: self.manager._spawn_holder(fixture, "synthetic-display")
        self.manager.spawner.run.side_effect = lambda job: job()
        child = mock.Mock(pid=744, returncode=None)
        child.wait.side_effect = subprocess.TimeoutExpired("synthetic original", 5)
        binds, ours, theirs, channel = (mock.Mock() for _ in range(4))
        binds.fds.return_value = (100, 101, 102, 103, 104)
        theirs.fileno.return_value = 301
        failure = OSError("synthetic pidfd open failure")
        with mock.patch.object(broker, "hold_binds", return_value=binds), mock.patch.object(
                broker.os, "pipe", return_value=(200, 201)), mock.patch.object(
                broker.socket, "socketpair", return_value=(ours, theirs)), mock.patch.object(
                broker, "RelayChannel", return_value=channel), mock.patch.object(
                broker, "holder_argv", return_value=["synthetic-no-execution"]), mock.patch.object(
                broker.subprocess, "Popen", return_value=child), mock.patch.object(
                broker.os, "pidfd_open", side_effect=failure), mock.patch.object(
                broker.os, "close"), mock.patch.object(broker, "remove_beneath", return_value=True), mock.patch.object(broker, "stopping"):
            with self.assertRaises(OSError) as caught:
                self.manager.start({"fixture": "demo", "checkout": self.checkout, "width": 960})
            self.assertIs(caught.exception, failure)
            retained = self.manager.live["demo"]
            self.assertIs(retained.process, child)
            self.assertFalse(retained.stopped)
            channel.close.assert_not_called()
            child.wait.assert_called_once()
            child.wait.side_effect = None
            child.wait.return_value = 0
            self.manager.stop(retained, {"keep": False})
            self.assertEqual(self.manager.live, {})
            channel.close.assert_called_once()

    def test_close_uncertainty_is_recorded_without_retrying_descriptor_numbers(self):
        self.fixture.spawn_resources = {"read_info": 200, "write_info": 201}
        with mock.patch.object(broker.os, "close", side_effect=[OSError("synthetic close uncertainty"), None]) as close:
            errors = self.manager._close_spawn_resources(self.fixture, ("read_info", "write_info"))
            self.assertEqual(len(errors), 1)
            self.assertEqual(self.fixture.spawn_resources, {})
            self.assertTrue(self.fixture.resource_errors)
            self.assertEqual(self.manager._close_spawn_resources(self.fixture, ("read_info", "write_info")), [])
            self.assertEqual(close.call_args_list, [mock.call(200), mock.call(201)])
        binds = broker.Binds(100, 101, 102, 103, 104)
        with mock.patch.object(broker.os, "close", side_effect=[OSError("synthetic bind close"), None, None, None, None]) as close:
            with self.assertRaises(OSError):
                binds.close()
            binds.close()
            self.assertEqual(close.call_args_list, [mock.call(value) for value in range(100, 105)])

    def test_host_wait_retains_first_timeout_or_cancel_until_positive_settlement(self):
        for original in (subprocess.TimeoutExpired("synthetic host", 3), KeyboardInterrupt()):
            with self.subTest(original=type(original).__name__):
                child = mock.Mock()
                child.wait.side_effect = [original, OSError("synthetic transient wait"), 0]
                streams = [io.BytesIO(), io.BytesIO()]
                with mock.patch.object(broker.tempfile, "TemporaryFile", side_effect=streams), mock.patch.object(
                        broker.subprocess, "Popen", return_value=child), mock.patch.object(
                        broker.time, "sleep"), mock.patch.object(broker, "stopping"):
                    with self.assertRaises(type(original)) as caught:
                        self.manager.harness(self.fixture.layout, ["synthetic-host-operation"], timeout=3)
                    self.assertIs(caught.exception, original)
                    self.assertEqual(child.wait.call_args_list, [mock.call(timeout=3), mock.call(), mock.call()])
                    self.assertEqual(child.kill.call_count, int(isinstance(original, subprocess.TimeoutExpired)))
                    self.assertTrue(all(stream.closed for stream in streams))
                    self.assertEqual(self.manager.host_operations, [])

    def test_positive_host_wait_returns_original_captured_output(self):
        child = mock.Mock()
        child.wait.return_value = 0
        streams = [io.BytesIO(b"synthetic stdout"), io.BytesIO(b"synthetic stderr")]
        with mock.patch.object(broker.tempfile, "TemporaryFile", side_effect=streams), mock.patch.object(
                broker.subprocess, "Popen", return_value=child):
            result = self.manager.harness(self.fixture.layout, ["synthetic-host-operation"], timeout=3)
        self.assertEqual((result.returncode, result.stdout, result.stderr), (0, b"synthetic stdout", b"synthetic stderr"))
        child.wait.assert_called_once_with(timeout=3)
        child.kill.assert_not_called()
        self.assertEqual(self.manager.host_operations, [])

    def test_uncertain_host_capture_close_remains_owned_without_second_close(self):
        child = mock.Mock()
        child.wait.return_value = 0
        streams = [mock.Mock(), mock.Mock()]
        streams[0].read.return_value = streams[1].read.return_value = b""
        failure = OSError("synthetic capture close")
        streams[0].close.side_effect = failure
        with mock.patch.object(broker.tempfile, "TemporaryFile", side_effect=streams), mock.patch.object(
                broker.subprocess, "Popen", return_value=child):
            with self.assertRaises(OSError) as caught:
                self.manager.harness(self.fixture.layout, ["synthetic-host-operation"], timeout=3)
        self.assertIs(caught.exception, failure)
        self.assertEqual(len(self.manager.host_operations), 1)
        self.assertTrue(self.manager.host_operations[0]["wait_settled"])
        self.assertTrue(self.manager.host_operations[0]["close_errors"])
        self.manager.live = {}
        self.assertTrue(self.manager.stop_all())
        for stream in streams:
            stream.close.assert_called_once()
        self.manager.spawner.close.assert_not_called()

    def test_partial_host_capture_acquisition_retains_a_failed_original_close(self):
        stream = mock.Mock()
        creation = OSError("synthetic second capture acquisition")
        stream.close.side_effect = OSError("synthetic first capture close")
        with mock.patch.object(broker.tempfile, "TemporaryFile", side_effect=[stream, creation]), mock.patch.object(
                broker.subprocess, "Popen") as child:
            with self.assertRaises(OSError) as caught:
                self.manager.harness(self.fixture.layout, ["synthetic-host-operation"])
        self.assertIs(caught.exception, creation)
        child.assert_not_called()
        stream.close.assert_called_once_with()
        operation, = self.manager.host_operations
        self.assertIsNone(operation["child"])
        self.assertEqual(operation["streams"], [stream])
        self.assertTrue(operation["close_errors"])


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
            (b'{"op":"status","fixture":"a;b","owner":"' + OWNER.encode() + b'"}\n', "invalid_request"),
            (b'{"op":"start","fixture":"a","checkout":"/etc"}\n', "path_escape"),
            (b'{"op":"cli","fixture":"a","owner":"' + OWNER.encode() + b'","args":["x"],"env":{"A":"b"}}\n', "invalid_request"),
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

    def test_another_users_connection_gets_nothing(self):
        left, right = socket.socketpair()
        self.addCleanup(left.close)
        self.addCleanup(right.close)
        self.assertEqual(broker.peer_uid(left), os.getuid())
        original = broker.peer_uid
        broker.peer_uid = lambda _connection: os.getuid() + 1
        self.addCleanup(setattr, broker, "peer_uid", original)
        connection = socket.socket(socket.AF_UNIX)
        connection.connect(str(self.path))
        with connection, connection.makefile("rwb") as stream:
            stream.write(STATUS)
            stream.flush()
            try:
                self.assertEqual(stream.readline(), b"")
            except ConnectionResetError:
                pass  # Closed with the request unread, which also answers nothing.
        self.assertEqual(self.seen, [])

    def test_every_connection_has_a_read_time_limit(self):
        self.assertEqual(broker.Connection.timeout, broker.CLIENT_READ_SECONDS)
        self.assertGreater(broker.CLIENT_READ_SECONDS, 0)

    def test_a_silent_client_does_not_keep_its_thread(self):
        original = broker.Connection.timeout
        broker.Connection.timeout = 0.2
        self.addCleanup(setattr, broker.Connection, "timeout", original)
        connection = socket.socket(socket.AF_UNIX)
        self.addCleanup(connection.close)
        connection.connect(str(self.path))
        deadline = time.monotonic() + 5
        connection.settimeout(5)
        self.assertEqual(connection.recv(1), b"", "the broker must close a silent client")
        self.assertLess(time.monotonic(), deadline)

    def test_connections_beyond_the_limit_are_turned_away(self):
        held = []
        self.addCleanup(lambda: [each.close() for each in held])
        for _ in range(broker.MAX_CONNECTIONS):
            each = socket.socket(socket.AF_UNIX)
            each.connect(str(self.path))
            held.append(each)
        time.sleep(0.2)
        extra = socket.socket(socket.AF_UNIX)
        held.append(extra)
        extra.settimeout(5)
        extra.connect(str(self.path))
        self.assertEqual(extra.recv(1), b"", "a connection past the limit must close at once")

    def test_a_failing_handler_does_not_end_the_broker(self):
        self.server.handler = lambda _request: 1 / 0
        self.assertEqual(self.exchange(STATUS)["code"], "internal_error")
        self.assertEqual(self.exchange(STATUS)["code"], "internal_error")

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
        result = self.client("cli", "--owner", OWNER, "demo", "--", "project", "open", "/x;y", **{AUTHORITY: token})
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(
            self.seen, [{"op": "cli", "fixture": "demo", "owner": OWNER, "args": ["project", "open", "/x;y"]}]
        )
        sent = raw[0] if isinstance(raw[0], bytes) else raw[0].encode()
        self.assertNotIn(token.encode(), sent)
        self.assertEqual(set(original(sent)), {"op", "fixture", "owner", "args"})

    def test_client_requests_match_what_the_broker_accepts(self):
        for arguments in (
            ["run-ui", "demo", "--owner", OWNER],
            ["status", "demo", "--owner", OWNER],
            ["stop", "demo", "--owner", OWNER, "--keep"],
            ["screenshot", "demo", "--owner", OWNER, "after-chat"],
            ["click", "demo", "--owner", OWNER, "Send", "--double", "--button", "right"],
            ["move", "demo", "--owner", OWNER, "100,200"],
            ["type", "demo", "--owner", OWNER, "hello; $(world)"],
            ["key", "demo", "--owner", OWNER, "ctrl+s", "Escape"],
            ["scroll", "demo", "--owner", OWNER, "v", "--at", "Send", "--", "-3"],
            ["wait", "demo", "--owner", OWNER, "Ready", "--gone", "--timeout", "5"],
            ["observe", "demo", "--owner", OWNER, "--no-text"],
            ["start", "demo", "--checkout", str(self.checkout), "--width", "480"],
            ["viewport", "demo", "--owner", OWNER, "--width", "480"],
            ["clear", "demo"],
        ):
            with self.subTest(arguments=arguments):
                result = self.client(*arguments)
                self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
                self.assertTrue(json.loads(result.stdout)["ok"], result.stdout)

    def test_client_reports_a_missing_broker_without_starting_one(self):
        with tempfile.TemporaryDirectory(prefix="nf-") as empty:
            result = self.client("status", "demo", "--owner", OWNER, XDG_RUNTIME_DIR=empty)
            self.assertEqual(result.returncode, 69)
            self.assertIn("do not start it from this shell", result.stderr)
            self.assertEqual(os.listdir(empty), [])

    def test_client_source_cannot_run_or_forward_anything(self):
        source = CLIENT.read_text()
        for forbidden in ("subprocess", "os.exec", "os.system", "os.spawn", "os.fork",
                          "os.environ.copy", "dict(os.environ)", AUTHORITY):
            self.assertNotIn(forbidden, source)


if __name__ == "__main__":
    unittest.main()
