#!/usr/bin/python3
"""The lane tasks' warning rule, without a compiler: scripts/cargo-lane runs a
stand-in gate that prints Cargo's JSON messages, and scripts/cargo_warnings.py
classifies messages as main's build sentinel does."""

import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import time
import unittest

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "scripts"))
import cargo_warnings

OWN_LIB = "path+file:///src#cagents@0.1.3"
OWN_UI = "path+file:///src/crates/cagents-ui#0.1.3"
VENDORED = "path+file:///src/vendor/libcosmic#1.0.0"
# Bounds only a hang: every wait it guards returns as soon as what it waits for happens.
HANG_GUARD = 60


def diagnostic(package_id, level, text, file_name="src/lib.rs", line=3):
    return json.dumps({
        "reason": "compiler-message", "package_id": package_id,
        "message": {"level": level, "message": text, "rendered": f"{level}: {text}\n",
                    "spans": [{"file_name": file_name, "line_start": line, "is_primary": True}]},
    }) + "\n"


# The stand-in gate prints the messages in $LANE_MESSAGES as Cargo's standard
# output, records its arguments, and exits with $LANE_STATUS.
GATE = """#!/usr/bin/python3
import json, os, signal, sys, time
with open(os.environ["LANE_CALLS"], "a") as calls:
    calls.write(json.dumps(sys.argv[1:]) + "\\n")
print("    Checking cagents", file=sys.stderr, flush=True)
sys.stdout.buffer.write(open(os.environ["LANE_MESSAGES"], "rb").read())
sys.stdout.flush()
if os.environ.get("LANE_WAIT"):
    # As the gate does: cancel the build, say so, and exit as the signal.
    def cancel(number, _frame):
        print(json.dumps({"reason": "compiler-message", "package_id": "path+file:///src#cagents@0.1.3",
                          "message": {"level": "error", "message": "cancelled",
                                      "rendered": f"error: cancelled by {number}\\n"}}), flush=True)
        sys.exit(128 + number)
    for number in (signal.SIGINT, signal.SIGTERM, signal.SIGHUP, signal.SIGQUIT):
        signal.signal(number, cancel)
    open(os.environ["LANE_WAIT"], "w").close()
    # Ended by the signal; longer than the test's hang guard, this bounds
    # only a test that never sends it.
    time.sleep(300)
sys.exit(int(os.environ.get("LANE_STATUS", "0")))
"""


class Classification(unittest.TestCase):
    def setUp(self):
        self.warnings = cargo_warnings.Warnings({"cagents", "cagents-ui"})

    def test_vendored_warnings_are_set_apart_and_own_ones_counted_once(self):
        kinds = [self.warnings.read(line)[0] for line in (
            diagnostic(VENDORED, "warning", "unused import"),
            diagnostic(OWN_LIB, "warning", "unused import: `Lane`"),
            diagnostic(OWN_LIB, "warning", "unused import: `Lane`"),
            diagnostic(OWN_UI, "warning", "function `probe` is never used", "crates/cagents-ui/src/a.rs", 9),
        )]
        self.assertEqual(kinds, [cargo_warnings.VENDORED, cargo_warnings.OWN, cargo_warnings.REPEAT,
                                 cargo_warnings.OWN])
        self.assertEqual(len(self.warnings.own), 2)
        self.assertEqual(len(self.warnings.vendored), 1)
        self.assertEqual(self.warnings.locations(), ["src/lib.rs:3", "crates/cagents-ui/src/a.rs:9"])
        self.assertEqual(self.warnings.rendered(),
                         "warning: unused import: `Lane`\nwarning: function `probe` is never used\n")

    def test_errors_and_notes_from_every_crate_are_diagnostics(self):
        for package_id in (OWN_LIB, VENDORED):
            for level in ("error", "error: internal compiler error", "note", "failure-note"):
                with self.subTest(package=package_id, level=level):
                    kind, text = self.warnings.read(diagnostic(package_id, level, "broken"))
                    self.assertEqual((kind, text), (cargo_warnings.DIAGNOSTIC, f"{level}: broken\n"))
        self.assertFalse(self.warnings.own or self.warnings.vendored)

    def test_without_the_workspace_no_warning_is_hidden_or_counted(self):
        warnings = cargo_warnings.Warnings(None)
        for package_id in (OWN_LIB, VENDORED):
            self.assertEqual(warnings.read(diagnostic(package_id, "warning", "w"))[0], cargo_warnings.DIAGNOSTIC)
        self.assertFalse(warnings.own or warnings.vendored)

    def test_cargo_messages_are_not_output_but_a_programs_lines_are(self):
        artifact = json.dumps({"reason": "compiler-artifact", "target": {"name": "cagents"}}) + "\n"
        self.assertEqual(self.warnings.read(artifact), (cargo_warnings.MESSAGE, None))
        for line in ("test result: ok. 1 passed\n", '{"type": "test", "event": "ok"}\n', "[1]\n"):
            with self.subTest(line=line):
                self.assertEqual(self.warnings.read(line), (cargo_warnings.OUTPUT, line))

    def test_a_diagnostic_without_rendering_shows_its_message(self):
        for rendered in (None, "", ["not", "text"], 7):
            with self.subTest(rendered=rendered):
                line = json.dumps({"reason": "compiler-message", "package_id": OWN_LIB,
                                   "message": {"level": "error", "message": "bare", "rendered": rendered}})
                self.assertEqual(self.warnings.read(line + "\n"), (cargo_warnings.DIAGNOSTIC, "bare\n"))

    def test_json_whose_reason_is_not_cargos_is_output(self):
        for line in ('{"reason": []}\n', '{"reason": {"a": 1}}\n', '{"reason": "verdict"}\n',
                     b'{"reason": 3}\n'):
            with self.subTest(line=line):
                self.assertEqual(self.warnings.read(line), (cargo_warnings.OUTPUT, line))

    def test_bytes_lines_come_back_unchanged(self):
        for line in (b"progress\r50%\r100%\n", b"\xff\xfe not utf-8\n"):
            with self.subTest(line=line):
                self.assertEqual(self.warnings.read(line), (cargo_warnings.OUTPUT, line))
        kind, text = self.warnings.read(diagnostic(VENDORED, "error", "broken").encode())
        self.assertEqual((kind, text), (cargo_warnings.DIAGNOSTIC, "error: broken\n"))

    def test_package_names_come_from_every_form_of_package_id(self):
        self.assertEqual(cargo_warnings.package_name(OWN_LIB), "cagents")
        self.assertEqual(cargo_warnings.package_name(OWN_UI), "cagents-ui")
        self.assertEqual(cargo_warnings.package_name("cagents 0.1.3 (path+file:///src)"), "cagents")

    def test_workspace_members_are_read_from_the_manifests(self):
        with tempfile.TemporaryDirectory(prefix="cargo-lane-", dir="/var/tmp") as temporary:
            root = Path(temporary)
            (root / "Cargo.toml").write_text('[workspace]\nmembers = [".", "crates/*"]\n'
                                             '[package]\nname = "cagents"\n')
            (root / "crates/ui").mkdir(parents=True)
            (root / "crates/ui/Cargo.toml").write_text('[package]\nname = "cagents-ui"\n')
            (root / "crates/empty").mkdir()
            self.assertEqual(cargo_warnings.workspace_packages(root), {"cagents", "cagents-ui"})
            self.assertEqual(cargo_warnings.workspace_root(root / "crates/ui"), root.resolve())
            self.assertEqual(cargo_warnings.workspace_root(root / "crates/empty"), root.resolve())
            (root / "Cargo.toml").write_text("[workspace\n")
            # A manifest that does not parse is where Cargo stops, too.
            (root / "crates/ui/Cargo.toml").write_text("[package\n")
            self.assertEqual(cargo_warnings.workspace_root(root / "crates/ui"), (root / "crates/ui").resolve())
            with self.assertRaises(cargo_warnings.WorkspaceUnreadable):
                cargo_warnings.workspace_packages(root)


class Lane(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory(prefix="cargo-lane-", dir="/var/tmp")
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        scripts = self.root / "scripts"
        scripts.mkdir()
        for name in ("cargo-lane", "cargo_warnings.py"):
            shutil.copy(ROOT / "scripts" / name, scripts / name)
        (scripts / "cargo-gate").write_text(GATE)
        (scripts / "cargo-gate").chmod(0o755)
        (self.root / "Cargo.toml").write_text('[workspace]\nmembers = [".", "crates/cagents-ui"]\n'
                                              '[package]\nname = "cagents"\n')
        (self.root / "crates/cagents-ui").mkdir(parents=True)
        (self.root / "crates/cagents-ui/Cargo.toml").write_text('[package]\nname = "cagents-ui"\n')
        self.messages = self.root / "messages"
        self.calls = self.root / "calls"

    def lane(self, *arguments, messages=(), status=0, **environment):
        self.messages.write_bytes(b"".join(m if isinstance(m, bytes) else m.encode() for m in messages))
        env = {key: value for key, value in os.environ.items() if key != "CAGENTS_VENDORED_WARNINGS"}
        env |= {"LANE_MESSAGES": str(self.messages), "LANE_CALLS": str(self.calls),
                "LANE_STATUS": str(status), **environment}
        return subprocess.run([str(self.root / "scripts/cargo-lane"), *arguments], cwd=self.root, env=env,
                              stdin=subprocess.DEVNULL, capture_output=True, text=True, timeout=HANG_GUARD)

    def recorded(self):
        return [json.loads(line) for line in self.calls.read_text().splitlines()]

    def test_a_clean_lane_passes_quietly_with_json_messages_requested_before_any_separator(self):
        result = self.lane("test", "--locked", "--lib", "--", "--exact", "x",
                           messages=["running 0 tests\n"])
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout, "running 0 tests\n")
        self.assertEqual(result.stderr, "    Checking cagents\n")
        self.assertEqual(self.recorded(), [["test", "--message-format=json", "--locked", "--lib",
                                            "--", "--exact", "x"]])

    def test_vendored_warnings_are_counted_not_shown(self):
        result = self.lane("check", messages=[diagnostic(VENDORED, "warning", f"vendored {n}")
                                              for n in range(3)])
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertNotIn("vendored 0", result.stderr)
        self.assertIn("3 warnings from vendored crates not shown", result.stderr)
        shown = self.lane("check", messages=[diagnostic(VENDORED, "warning", "vendored 0")],
                          CAGENTS_VENDORED_WARNINGS="1")
        self.assertEqual(shown.returncode, 0)
        self.assertIn("warning: vendored 0\n", shown.stderr)
        self.assertNotIn("not shown", shown.stderr)

    def test_a_workspace_warning_shows_once_and_fails_a_lane_that_compiled(self):
        own = diagnostic(OWN_UI, "warning", "function `probe` is never used", "crates/cagents-ui/src/a.rs", 9)
        result = self.lane("check", messages=[own, own, diagnostic(VENDORED, "warning", "vendored")])
        self.assertEqual(result.returncode, 1)
        self.assertEqual(result.stderr.count("function `probe` is never used"), 1)
        self.assertTrue(result.stderr.endswith(
            "cargo-lane: 1 warning from the workspace's crates (crates/cagents-ui/src/a.rs:9); "
            "main's build check fails on them\n"), result.stderr)

    def test_a_build_that_allows_warnings_shows_them_and_keeps_cargos_status(self):
        own = diagnostic(OWN_UI, "warning", "function `probe` is never used", "crates/cagents-ui/src/a.rs", 9)
        result = self.lane("--allow-warnings", "build", "--bin", "cagents-ui",
                           messages=[own, diagnostic(VENDORED, "warning", "vendored")])
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stderr.count("function `probe` is never used"), 1)
        self.assertNotIn("warning: vendored", result.stderr)
        self.assertIn("1 warning from vendored crates not shown", result.stderr)
        self.assertIn("1 warning from the workspace's crates (crates/cagents-ui/src/a.rs:9)", result.stderr)
        self.assertEqual(self.recorded(), [["build", "--message-format=json", "--bin", "cagents-ui"]])
        failed = self.lane("--allow-warnings", "build", status=101, messages=[own])
        self.assertEqual(failed.returncode, 101)

    def test_another_gate_can_run_the_build(self):
        other = self.root / "other-gate"
        shutil.copy(self.root / "scripts/cargo-gate", other)
        result = self.lane("--allow-warnings", "--gate", str(other), "build", "--bin", "cagents",
                           messages=[diagnostic(VENDORED, "warning", "vendored")])
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertNotIn("warning: vendored", result.stderr)
        self.assertEqual(self.recorded(), [["build", "--message-format=json", "--bin", "cagents"]])
        (self.root / "scripts/cargo-gate").unlink()
        self.assertEqual(self.lane("--gate", str(other), "check").returncode, 0)
        self.assertEqual(self.lane("--gate").returncode, 2)

    def test_the_workspace_is_the_one_cargo_uses_from_the_current_directory(self):
        # Another checkout, as native-evidence builds a side with this one's tools.
        other = self.root / "other"
        (other / "src").mkdir(parents=True)
        (other / "Cargo.toml").write_text('[workspace]\n[package]\nname = "older"\n')
        own = diagnostic("path+file:///other#older@0.1.0", "warning", "unused import: `Old`")
        self.messages.write_text(own + diagnostic(OWN_LIB, "warning", "unused import: `Lane`"))
        env = {key: value for key, value in os.environ.items() if key != "CAGENTS_VENDORED_WARNINGS"}
        env |= {"LANE_MESSAGES": str(self.messages), "LANE_CALLS": str(self.calls)}
        result = subprocess.run([str(self.root / "scripts/cargo-lane"), "--allow-warnings", "build"],
                                cwd=other / "src", env=env, stdin=subprocess.DEVNULL, capture_output=True,
                                text=True, timeout=HANG_GUARD)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("unused import: `Old`", result.stderr)
        self.assertNotIn("unused import: `Lane`", result.stderr)
        self.assertIn("1 warning from vendored crates not shown", result.stderr)

    def test_errors_from_every_crate_show_and_keep_cargos_status(self):
        result = self.lane("check", status=101, messages=[
            diagnostic(VENDORED, "error", "vendored crate broke"),
            diagnostic(OWN_LIB, "warning", "unused"),
            diagnostic(OWN_LIB, "error", "own crate broke"),
        ])
        self.assertEqual(result.returncode, 101)
        self.assertIn("error: vendored crate broke\n", result.stderr)
        self.assertIn("error: own crate broke\n", result.stderr)
        self.assertIn("1 warning from the workspace's crates", result.stderr)

    def test_short_messages_stay_short_and_json_is_left_to_the_caller(self):
        self.assertEqual(self.lane("check", "--message-format", "short", "--lib").returncode, 0)
        raw = diagnostic(VENDORED, "warning", "vendored")
        result = self.lane("check", "--message-format=json", "--lib", messages=[raw])
        self.assertEqual(result.returncode, 0)
        self.assertEqual(result.stdout, raw)
        self.assertEqual(self.recorded(), [
            ["check", "--message-format=json,json-diagnostic-short", "--lib"],
            ["check", "--message-format=json", "--lib"],
        ])

    def test_a_format_cargo_would_refuse_reaches_cargo_unchanged(self):
        for arguments in (["--message-format=bogus"], ["--lib", "--message-format"],
                          ["--message-format", "short,bogus"]):
            with self.subTest(arguments=arguments):
                self.calls.unlink(missing_ok=True)
                self.lane("check", *arguments)
                self.assertEqual(self.recorded(), [["check", *arguments]])

    def test_a_terminal_keeps_colour_short_or_not(self):
        import pty
        for arguments, expected in (([], "json,json-diagnostic-rendered-ansi"),
                                    (["--message-format=short"],
                                     "json,json-diagnostic-short,json-diagnostic-rendered-ansi")):
            with self.subTest(arguments=arguments):
                self.calls.unlink(missing_ok=True)
                self.messages.write_text("")
                controller, terminal = pty.openpty()
                env = os.environ | {"LANE_MESSAGES": str(self.messages), "LANE_CALLS": str(self.calls)}
                try:
                    subprocess.run([str(self.root / "scripts/cargo-lane"), "check", *arguments],
                                   cwd=self.root, env=env, stdin=subprocess.DEVNULL,
                                   stdout=subprocess.DEVNULL, stderr=terminal, timeout=HANG_GUARD, check=True)
                finally:
                    os.close(terminal)
                    os.close(controller)
                self.assertEqual(self.recorded(), [["check", f"--message-format={expected}"]])

    def test_program_output_passes_byte_for_byte(self):
        output = b"progress\r50%\r100%\n\xff not utf-8\n"
        self.messages.write_bytes(output)
        env = os.environ | {"LANE_MESSAGES": str(self.messages), "LANE_CALLS": str(self.calls)}
        result = subprocess.run([str(self.root / "scripts/cargo-lane"), "test"], cwd=self.root, env=env,
                                stdin=subprocess.DEVNULL, capture_output=True, timeout=HANG_GUARD)
        self.assertEqual(result.returncode, 0)
        self.assertEqual(result.stdout, output)

    def test_the_closing_line_locates_ten_warnings_and_counts_the_rest(self):
        result = self.lane("check", messages=[diagnostic(OWN_LIB, "warning", f"unused {n}", "src/lib.rs", n)
                                              for n in range(12)])
        self.assertEqual(result.returncode, 1)
        last = result.stderr.splitlines()[-1]
        self.assertIn("12 warnings from the workspace's crates (src/lib.rs:0, ", last)
        self.assertIn("src/lib.rs:9, and 2 more)", last)
        self.assertNotIn("src/lib.rs:10", last)

    def test_a_signal_reaches_the_gate_whose_status_and_output_are_kept(self):
        import signal
        for number in (signal.SIGTERM, signal.SIGINT, signal.SIGHUP, signal.SIGQUIT):
            with self.subTest(signal=number.name):
                ready = self.root / f"ready-{number}"
                self.messages.write_text(diagnostic(OWN_LIB, "warning", "unused"))
                env = os.environ | {"LANE_MESSAGES": str(self.messages), "LANE_CALLS": str(self.calls),
                                    "LANE_WAIT": str(ready)}
                # Its own group, so a lane that never forwards the signal can be ended with its gate.
                with subprocess.Popen([str(self.root / "scripts/cargo-lane"), "check"], cwd=self.root,
                                      env=env, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE,
                                      stderr=subprocess.PIPE, text=True, start_new_session=True) as lane:
                    deadline = time.monotonic() + HANG_GUARD
                    while not ready.exists():
                        self.assertLess(time.monotonic(), deadline, "the gate never got ready")
                        time.sleep(0.05)
                    # Only the lane: the gate must hear it from the lane.
                    lane.send_signal(number)
                    try:
                        _, stderr = lane.communicate(timeout=HANG_GUARD)
                    except subprocess.TimeoutExpired:
                        os.killpg(lane.pid, signal.SIGKILL)
                        self.fail("the signal never ended the lane")
                self.assertEqual(lane.returncode, 128 + number, stderr)
                self.assertIn(f"error: cancelled by {int(number)}\n", stderr)
                self.assertIn("1 warning from the workspace's crates", stderr)

    def test_an_unreadable_workspace_shows_every_warning_and_fails_on_none(self):
        (self.root / "Cargo.toml").write_text("[workspace\n")
        result = self.lane("check", messages=[diagnostic(VENDORED, "warning", "vendored"),
                                              diagnostic(OWN_LIB, "warning", "own")])
        self.assertEqual(result.returncode, 0)
        self.assertIn("warning: vendored\n", result.stderr)
        self.assertIn("warning: own\n", result.stderr)
        self.assertIn("unreadable", result.stderr)

    def test_without_a_subcommand_it_explains_its_usage(self):
        self.assertEqual(self.lane().returncode, 2)
        self.assertEqual(self.lane("--help").returncode, 0)
        self.assertFalse(self.calls.exists())


if __name__ == "__main__":
    unittest.main(verbosity=2)
