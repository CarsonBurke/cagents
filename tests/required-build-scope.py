#!/usr/bin/env python3
"""Workflow regressions using private fake tools, never a real scope or Cargo.

These black-box tests are authored source until separately admitted execution.
The fake helper's exec models routing only; it proves no kernel/manager property.
Only fixed synthetic markers, arguments, cwd and streams enter fixture logs.
"""

import hashlib
import json
import os
from pathlib import Path
import runpy
import shutil
import subprocess
import sys
import tempfile
import unittest
from unittest import mock


SOURCE = Path(__file__).resolve().parents[1]
FAKE = r'''
import json
import os
from pathlib import Path
import sys

name = Path(sys.argv[0]).name
args = sys.argv[1:]

def event(kind, **fields):
    record = dict(kind=kind, args=args, cwd=os.getcwd(),
                  marker=os.environ['FIXTURE_CONTEXT'], **fields)
    with open(os.environ['FIXTURE_EVENT_LOG'], 'a') as file:
        file.write(json.dumps(record) + '\n')

if name == 'zig':
    assert args == ['env']
    print(json.dumps(dict(zig_exe=sys.argv[0], version='0.15.2')))
elif name == 'scope-helper':
    event('helper')
    if os.environ.get('FIXTURE_HELPER_REFUSE'):
        print('fixture-helper-refused', file=sys.stderr)
        sys.exit(63)
    assert args[:2] == ['--internal-build-scope-exec', '--']
    print('fixture-helper-stderr', file=sys.stderr)
    os.execv(args[2], ['cargo', *args[3:]])
elif name == 'cargo':
    received = sys.stdin.read() if os.environ.get('FIXTURE_READ_STDIN') else None
    event('cargo', stdin=received)
    if os.environ.get('FIXTURE_CARGO_EXIT'):
        sys.exit(int(os.environ['FIXTURE_CARGO_EXIT']))
    assert args[0] == 'build'
    product = args[args.index('--bin') + 1] if '--bin' in args else 'ceditor'
    if os.environ.get('FIXTURE_FAIL_BIN') == product:
        print('fixture-cargo-failed', file=sys.stderr)
        sys.exit(47)
    if os.environ.get('FIXTURE_MUTATE_HELPER_AFTER') == product:
        with open(os.environ['FIXTURE_HELPER_PATH'], 'a') as file:
            file.write('\n# fixture replacement between build preflights\n')
    print('fixture-unexpected-build-output')
    print(json.dumps(dict(reason='compiler-message', message=dict(rendered='fixture-rendered\n'))))
    print(json.dumps(dict(reason='compiler-artifact', target=dict(name=product, kind=['lib']), executable='/not/a/bin')))
    print(json.dumps(dict(reason='compiler-artifact', target=dict(name=product, kind=['bin']),
                         executable=str(Path(os.environ['FIXTURE_PRODUCTS']) / (product + os.environ.get('FIXTURE_ARTIFACT_SUFFIX', ''))))))
    print(json.dumps(dict(reason='build-finished', success=True)))
    print('fixture-cargo-stderr', file=sys.stderr)
elif name in ('cagents', 'cagentsd', 'cagents-ui', 'ceditor'):
    kind = {'cagents': 'cli', 'cagentsd': 'daemon', 'cagents-ui': 'ui', 'ceditor': 'editor'}[name]
    received = sys.stdin.read() if os.environ.get('FIXTURE_UI_READ_STDIN') else None
    event(kind, stdin=received)
    print('fixture-product-stdout')
    print('fixture-product-stderr', file=sys.stderr)
    sys.exit(int(os.environ.get('FIXTURE_PRODUCT_EXIT', '0')))
elif name == 'hot-development':
    event('hot')
    sys.exit(91)
else:
    assert name in ('install', 'gtk-update-icon-cache', 'update-desktop-database')
'''


class RequiredBuildScope(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory(prefix="required-build-scope-")
        self.addCleanup(temporary.cleanup)
        self.directory = Path(temporary.name)
        self.checkout = self.directory / "checkout with spaces"
        (self.checkout / "scripts").mkdir(parents=True)
        for name in ("run-development", "run-ceditor-development", "build-ceditor-product"):
            shutil.copy2(SOURCE / "scripts" / name, self.checkout / "scripts" / name)
        (self.checkout / "mise.toml").write_text('[tools]\nzig = "0.15.2"\n')
        packaging = self.checkout / "packaging"
        packaging.mkdir()
        (packaging / "io.github.cagents.ceditor.svg").write_text("fixture-svg\n")
        (packaging / "io.github.cagents.ceditor.desktop").write_text(
            "[Desktop Entry]\nName=fixture\nExec=fixture\nIcon=fixture\n"
        )
        self.bin = self.directory / "fake bin"
        self.products = self.directory / "selected products"
        self.bin.mkdir()
        self.products.mkdir()
        for name in ("cargo", "zig", "install", "gtk-update-icon-cache", "update-desktop-database"):
            self.fake(self.bin / name)
        self.fake(self.checkout / "scripts/hot-development")
        for name in ("cagents", "cagentsd", "cagents-ui", "ceditor"):
            self.fake(self.products / name)
        self.helper = self.directory / "stable helper" / "scope-helper"
        self.helper.parent.mkdir()
        self.fake(self.helper)
        self.digest = hashlib.sha256(self.helper.read_bytes()).hexdigest()
        self.caller = self.directory / "original caller"
        self.caller.mkdir()
        self.events = self.directory / "events.jsonl"
        home = self.directory / "private-home"
        data = self.directory / "private-data"
        runtime = self.directory / "private-runtime"
        home.mkdir()
        (data / "applications").mkdir(parents=True)
        runtime.mkdir()
        # Deliberately construct only synthetic fixture values. No inherited
        # environment, account authority or user data is read, copied or logged.
        self.environment = {
            "PATH": f"{self.bin}:/usr/bin:/bin", "HOME": str(home),
            "XDG_DATA_HOME": str(data), "XDG_RUNTIME_DIR": str(runtime),
            "CAGENTS_ZIG": str(self.bin / "zig"),
            "FIXTURE_CONTEXT": "synthetic-opaque-marker",
            "FIXTURE_EVENT_LOG": str(self.events), "FIXTURE_PRODUCTS": str(self.products),
            "FIXTURE_HELPER_PATH": str(self.helper),
        }

    def fake(self, path):
        path.write_text(f"#!{sys.executable}\n" + FAKE)
        path.chmod(0o755)

    def flags(self):
        return ["--build-scope-helper", str(self.helper), "--build-scope-sha256", self.digest]

    def invoke(self, script, arguments, **changes):
        environment = dict(self.environment, **changes)
        result = subprocess.run(
            [str(self.checkout / "scripts" / script), *arguments],
            cwd=self.caller, env=environment, input="fixture-input\n",
            capture_output=True, text=True, timeout=10,
        )
        events = [json.loads(line) for line in self.events.read_text().splitlines()] if self.events.exists() else []
        return result, events

    def reset_events(self):
        if self.events.exists():
            self.events.unlink()

    def test_invalid_configuration_refuses_before_any_cargo_or_helper(self):
        alias = self.directory / "helper-alias"
        alias.symlink_to(self.helper)
        fifo = self.directory / "helper-fifo"
        os.mkfifo(fifo)
        cases = [
            ["--build-scope-helper", str(self.helper)],
            ["--build-scope-sha256", self.digest],
            ["--build-scope-helper", "relative/helper", "--build-scope-sha256", self.digest],
            ["--build-scope-helper", str(self.helper), "--build-scope-sha256", "not-a-hash"],
            ["--build-scope-helper", str(self.helper), "--build-scope-sha256", "0" * 64],
            ["--build-scope-helper", str(self.directory / "absent"), "--build-scope-sha256", self.digest],
            ["--build-scope-helper", str(alias), "--build-scope-sha256", self.digest],
            ["--build-scope-helper", str(fifo), "--build-scope-sha256", self.digest],
            [*self.flags(), "--build-scope-helper", str(self.helper)],
        ]
        for flags in cases:
            with self.subTest(flags=flags):
                result, events = self.invoke("run-development", ["--browserless", *flags])
                self.assertNotEqual(result.returncode, 0, result.stderr)
                self.assertEqual(events, [])

    def test_three_isolated_graphs_launch_selected_ui_without_rebuilding(self):
        for browserless in (True, False):
            with self.subTest(browserless=browserless):
                self.reset_events()
                prefix = ["--browserless"] if browserless else []
                result, events = self.invoke(
                    "run-development", [*prefix, *self.flags(), "--", "file with spaces", "--scratch"],
                    FIXTURE_PRODUCT_EXIT="17", FIXTURE_UI_READ_STDIN="1",
                )
                self.assertEqual(result.returncode, 17, result.stderr)
                self.assertEqual([e["kind"] for e in events], ["helper", "cargo"] * 3 + ["ui"])
                cargo = [e for e in events if e["kind"] == "cargo"]
                features = ["development", "development,host", "development,ui" if browserless else "development,integrated-browser"]
                for event, name, feature in zip(cargo, ("cagents", "cagentsd", "cagents-ui"), features):
                    expected = ["build", "--locked", "--profile", "dev-runtime"]
                    if browserless:
                        expected.append("--no-default-features")
                    expected += ["--features", feature, "--bin", name, "--message-format=json-render-diagnostics"]
                    self.assertEqual(event["args"], expected)
                self.assertEqual(events[-1]["args"], ["file with spaces", "--scratch"])
                self.assertEqual(events[-1]["stdin"], "fixture-input\n")
                self.assertTrue(all(e["cwd"] == str(self.checkout) for e in events))
                self.assertTrue(all(e["marker"] == "synthetic-opaque-marker" for e in events))
                self.assertEqual(result.stdout, "fixture-product-stdout\n")
                self.assertIn("fixture-product-stderr", result.stderr)

    def test_fresh_daemon_uses_selected_cli_only_after_all_builds(self):
        result, events = self.invoke("run-development", ["--browserless", *self.flags(), "--fresh-daemon"])
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual([e["kind"] for e in events], ["helper", "cargo"] * 3 + ["cli", "ui"])
        self.assertEqual(events[-2]["args"], ["daemon", "stop-for-rebuild", "--including-subagents"])
        self.reset_events()
        result, events = self.invoke(
            "run-development", ["--browserless", *self.flags(), "--fresh-daemon"], FIXTURE_FAIL_BIN="cagentsd",
        )
        self.assertEqual(result.returncode, 47, result.stderr)
        self.assertEqual([e["kind"] for e in events], ["helper", "cargo"] * 2)

    def test_helper_refusal_has_no_unisolated_fallback(self):
        result, events = self.invoke(
            "run-development", ["--browserless", *self.flags(), "--fresh-daemon"], FIXTURE_HELPER_REFUSE="1",
        )
        self.assertEqual(result.returncode, 63, result.stderr)
        self.assertEqual([e["kind"] for e in events], ["helper"])
        self.assertIn("fixture-helper-refused", result.stderr)

    def test_misplaced_options_refuse_before_editor_or_selector_build(self):
        result, events = self.invoke("run-ceditor-development", ["--review-alerts", *self.flags()])
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(events, [])
        result, events = self.invoke("build-ceditor-product", ["ceditor", *self.flags(), "--locked"])
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(events, [])

    def test_helper_is_rehashed_before_each_build(self):
        result, events = self.invoke(
            "run-development", ["--browserless", *self.flags(), "--fresh-daemon"], FIXTURE_MUTATE_HELPER_AFTER="cagents",
        )
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("SHA256 does not match", result.stderr)
        self.assertEqual([e["kind"] for e in events], ["helper", "cargo"])

    def test_digest_preflight_never_requests_an_unbounded_read(self):
        # Future test execution loads definitions only (not __main__). The
        # wrapped file is this tiny private fake, never a real helper ELF.
        namespace = runpy.run_path(str(self.checkout / "scripts/build-ceditor-product"))
        original_fdopen = os.fdopen
        requested = []
        case = self

        class BoundedReader:
            def __init__(self, descriptor, mode):
                self.file = original_fdopen(descriptor, mode)

            def __enter__(self):
                return self

            def __exit__(self, *exception):
                self.file.close()

            def fileno(self):
                return self.file.fileno()

            def read(self, size=-1):
                case.assertGreater(size, 0)
                case.assertLessEqual(size, 1024 * 1024)
                requested.append(size)
                return self.file.read(size)

        values = {"--build-scope-helper": str(self.helper), "--build-scope-sha256": self.digest}
        with mock.patch.object(namespace["os"], "fdopen", BoundedReader), mock.patch.object(
            namespace["shutil"], "which", return_value=str(self.bin / "cargo")
        ):
            command = namespace["required_build_command"](values, ["build", "--locked"])
        self.assertTrue(requested)
        self.assertEqual(command, [str(self.helper), "--internal-build-scope-exec", "--", str(self.bin / "cargo"), "build", "--locked"])

    def test_selector_preserves_build_arguments_streams_and_original_failure(self):
        arguments = [*self.flags(), "ceditor", "--locked", "--profile", "dev-runtime", "--package", "ceditor", "--features", "development", "--jobs", "2"]
        arguments += ["--target-dir", os.fsdecode(b"synthetic-target-\xff")]
        result, events = self.invoke("build-ceditor-product", arguments, FIXTURE_READ_STDIN="1")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout, str(self.products / "ceditor") + "\n")
        self.assertEqual([e["kind"] for e in events], ["helper", "cargo"])
        self.assertEqual(events[0]["args"], ["--internal-build-scope-exec", "--", str(self.bin / "cargo"), *events[1]["args"]])
        self.assertEqual(events[1]["args"], ["build", *arguments[len(self.flags()) + 1:], "--message-format=json-render-diagnostics"])
        self.assertEqual(events[1]["stdin"], "fixture-input\n")
        self.assertEqual(events[1]["cwd"], str(self.caller))
        self.assertEqual(events[1]["marker"], "synthetic-opaque-marker")
        for marker in ("fixture-helper-stderr", "fixture-cargo-stderr", "fixture-rendered", "fixture-unexpected-build-output"):
            self.assertIn(marker, result.stderr)
        self.reset_events()
        result, events = self.invoke("build-ceditor-product", arguments, FIXTURE_FAIL_BIN="ceditor")
        self.assertEqual(result.returncode, 47)
        self.assertEqual(result.stdout, "")
        self.assertEqual([e["kind"] for e in events], ["helper", "cargo"])

    def test_hot_modes_refuse_before_hot_runner_or_build(self):
        for hot in ("--hot", "--hot-integrated"):
            with self.subTest(hot=hot):
                result, events = self.invoke("run-development", [hot, *self.flags()])
                self.assertEqual(result.returncode, 2)
                self.assertIn("does not support hot reload", result.stderr)
                self.assertEqual(events, [])

    def test_newline_artifact_path_refuses_before_daemon_stop_or_launch(self):
        # Both the real selected path and the newline-stripped wrong path exist.
        # A shell substitution must never quietly choose the latter.
        self.fake(self.products / "cagents\n")
        result, events = self.invoke(
            "run-development", ["--browserless", *self.flags(), "--fresh-daemon"], FIXTURE_ARTIFACT_SUFFIX="\n",
        )
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("single-line path interface", result.stderr)
        self.assertEqual([e["kind"] for e in events], ["helper", "cargo"])

    def test_editor_forwards_mode_and_launches_from_original_caller(self):
        for alerts in (False, True):
            with self.subTest(alerts=alerts):
                self.reset_events()
                prefix = ["--review-alerts"] if alerts else []
                editor_arguments = ["file with spaces", "--build-scope-helper", "editor argument", "--build-scope-sha256", "literal"]
                result, events = self.invoke(
                    "run-ceditor-development", [*self.flags(), *prefix, "--", *editor_arguments], FIXTURE_PRODUCT_EXIT="19",
                )
                self.assertEqual(result.returncode, 19, result.stderr)
                self.assertEqual([e["kind"] for e in events], ["helper", "cargo"] * (3 if alerts else 1) + ["editor"])
                self.assertEqual(events[-1]["args"], editor_arguments)
                self.assertEqual(events[-1]["cwd"], str(self.caller))
                self.assertTrue(all(e["cwd"] == str(self.checkout) for e in events[:-1]))
                self.assertTrue(all(e["marker"] == "synthetic-opaque-marker" for e in events))

    def test_ordinary_runner_retains_its_first_direct_cargo_build(self):
        result, events = self.invoke("run-development", ["--browserless"], FIXTURE_CARGO_EXIT="83")
        self.assertEqual(result.returncode, 83)
        self.assertEqual([e["kind"] for e in events], ["cargo"])
        self.assertEqual(events[0]["args"], ["build", "--locked", "--profile", "dev-runtime", "--no-default-features", "--features", "development", "--bin", "cagents"])


if __name__ == "__main__":
    unittest.main()
