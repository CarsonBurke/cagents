#!/usr/bin/env python3
"""No Cargo/provider calls: exercise orchestration, identity and Linux lock mount."""
from contextlib import ExitStack, contextmanager
import importlib.machinery
import importlib.util
import errno
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
from types import SimpleNamespace
import unittest
from unittest import mock

sys.dont_write_bytecode = True
source = Path(__file__).resolve().parents[1] / "scripts/hot-development"
loader = importlib.machinery.SourceFileLoader("hot_development", str(source))
spec = importlib.util.spec_from_loader(loader.name, loader)
hot = importlib.util.module_from_spec(spec)
loader.exec_module(hot)


class HotRunner(unittest.TestCase):
    @contextmanager
    def orchestration(self, arguments, *, failed_build=None, failed_metadata=False, missing_artifact=None):
        fixtures = source.parents[1] / ".worktrees"
        fixtures.mkdir(exist_ok=True)
        with tempfile.TemporaryDirectory(prefix="hot-run-test-", dir=fixtures) as directory, ExitStack() as stack:
            root = Path(directory)
            target = root / "configured-target"
            calls = []

            def build(command, **kwargs):
                calls.append(("build", command))
                name = command[command.index("--bin") + 1]
                child = mock.MagicMock()
                child.__enter__.return_value = child
                child.stdout = [] if name == missing_artifact else [json.dumps({
                    "reason": "compiler-artifact", "target": {"name": name},
                    "executable": str(root / "products" / name),
                }) + "\n"]
                child.wait.return_value = int(name == failed_build)
                return child

            def check_output(command, **kwargs):
                if command == ["rustc", "-vV"]:
                    return "rustc 1.93.0\nhost: x86_64-unknown-linux-gnu\n"
                calls.append(("metadata", command))
                if failed_metadata:
                    raise subprocess.CalledProcessError(101, command)
                return json.dumps({"target_directory": str(target)})

            def stage(product, destination):
                calls.append(("stage", product, destination))

            state = SimpleNamespace(root=root, target=target, calls=calls)
            stack.enter_context(mock.patch.object(hot, "ROOT", root))
            stack.enter_context(mock.patch.object(sys, "argv", [str(source), *arguments]))
            state.prerequisites = stack.enter_context(mock.patch.object(hot, "prerequisites", return_value="/tools/bin/cargo-hot"))
            stack.enter_context(mock.patch.object(hot.shutil, "which", return_value="/tools/bin/bwrap"))
            state.build = stack.enter_context(mock.patch.object(hot.subprocess, "Popen", side_effect=build))
            state.metadata = stack.enter_context(mock.patch.object(hot.subprocess, "check_output", side_effect=check_output))
            state.stage = stack.enter_context(mock.patch.object(hot, "stage_product", side_effect=stage))
            state.execute = stack.enter_context(mock.patch.object(hot.os, "execvp"))
            state.run = stack.enter_context(mock.patch.object(hot.subprocess, "run", return_value=SimpleNamespace(returncode=23)))
            yield state

    def assert_locked_sandbox(self, command, root):
        position = command.index("--ro-bind")
        self.assertEqual(command[position + 1:position + 3], [str(root / "Cargo.lock")] * 2)
        self.assertEqual(command[command.index("--chdir") + 1], str(root))
        return command[command.index("--") + 1:]

    def test_prepares_only_cli_and_daemon_then_resolves_selected_ui_before_offline_hot(self):
        for browserless in (True, False):
            with self.subTest(browserless=browserless), self.orchestration(["--browserless"] if browserless else []) as state:
                self.assertIsNone(hot.main())
                state.prerequisites.assert_called_once_with()
                self.assertEqual([call[0] for call in state.calls], ["build", "build", "metadata", "stage", "stage"])
                identity = "development" if browserless else "development,full"
                for call, name, features in zip(state.calls, ("cagents", "cagentsd"), (identity, identity + ",host")):
                    command = self.assert_locked_sandbox(call[1], state.root)
                    self.assertEqual(command, [
                        "cargo", "build", "--locked", "--profile", "dev-runtime",
                        "--target", "x86_64-unknown-linux-gnu", "--no-default-features",
                        "--features", features, "--bin", name, "--message-format", "json-render-diagnostics",
                    ])
                    self.assertNotIn("CARGO_NET_OFFLINE", call[1])
                ui_features = "hot-reload" if browserless else "hot-reload,integrated-browser"
                metadata = state.calls[2][1]
                self.assertEqual(self.assert_locked_sandbox(metadata, state.root), [
                    "cargo", "metadata", "--locked", "--no-default-features", "--features", ui_features,
                    "--filter-platform", "x86_64-unknown-linux-gnu", "--format-version", "1",
                ])
                self.assertNotIn("CARGO_NET_OFFLINE", metadata)
                bundle = state.target / "cargo-hot" / "cagents-ui-hot" / "dev-runtime"
                self.assertEqual([call[2] for call in state.calls[3:]], [bundle / "cagents", bundle / "cagentsd"])
                self.assertEqual(os.readlink(bundle / "cagents-ui"), "cagents-ui-hot")
                state.execute.assert_called_once()
                executable, launch = state.execute.call_args.args
                self.assertEqual(executable, "/tools/bin/bwrap")
                self.assertEqual(self.assert_locked_sandbox(launch, state.root), [
                    "/tools/bin/cargo-hot", "hot", "--profile", "dev-runtime", "--no-default-features",
                    "--features", ui_features, "--bin", "cagents-ui-hot", "--",
                ])
                self.assertIn(["--setenv", "CARGO_NET_OFFLINE", "true"], [launch[i:i + 3] for i in range(len(launch))])
                self.assertIn(["--unsetenv", "CAGENTS_HOT_FRESH_DAEMON"], [launch[i:i + 2] for i in range(len(launch))])
                state.run.assert_not_called()

    def test_preparation_failure_never_stages_or_launches(self):
        for failure in ("cagents", "cagentsd", "metadata"):
            with self.subTest(failure=failure), self.orchestration(
                    ["--fresh-daemon"], failed_build=failure, failed_metadata=failure == "metadata") as state:
                with self.assertRaises((RuntimeError, subprocess.CalledProcessError)):
                    hot.main()
                state.stage.assert_not_called()
                state.execute.assert_not_called()
                state.run.assert_not_called()
                self.assertFalse(state.target.exists())
                self.assertEqual([call[0] for call in state.calls], {
                    "cagents": ["build"], "cagentsd": ["build", "build"],
                    "metadata": ["build", "build", "metadata"],
                }[failure])

    def test_missing_companion_artifact_never_resolves_stages_or_launches(self):
        for missing in ("cagents", "cagentsd"):
            with self.subTest(missing=missing), self.orchestration(
                    ["--fresh-daemon"], missing_artifact=missing) as state:
                with self.assertRaises(RuntimeError) as failure:
                    hot.main()
                self.assertEqual(str(failure.exception),
                                 f"Locked {missing} build failed; development daemon was not stopped")
                state.prerequisites.assert_called_once_with()
                names = ["cagents"] if missing == "cagents" else ["cagents", "cagentsd"]
                self.assertEqual([call[0] for call in state.calls], ["build"] * len(names))
                for call, name in zip(state.calls, names):
                    command = self.assert_locked_sandbox(call[1], state.root)
                    self.assertEqual(command, [
                        "cargo", "build", "--locked", "--profile", "dev-runtime",
                        "--target", "x86_64-unknown-linux-gnu", "--no-default-features",
                        "--features", "development,full" + (",host" if name == "cagentsd" else ""),
                        "--bin", name, "--message-format", "json-render-diagnostics",
                    ])
                state.metadata.assert_called_once_with(["rustc", "-vV"], text=True)
                state.stage.assert_not_called()
                state.execute.assert_not_called()
                state.run.assert_not_called()
                self.assertFalse(state.target.exists())

    def test_fresh_daemon_and_ui_arguments_are_forwarded_only_to_hot_launch(self):
        arguments = ["--device", "device with spaces", "--new-window", "--log-file", "ui.log"]
        with self.orchestration(["--browserless", "--fresh-daemon", "--", *arguments]) as state:
            hot.main()
            launch = state.execute.call_args.args[1]
            self.assertEqual(launch[-len(arguments):], arguments)
            self.assertIn(["--setenv", "CAGENTS_HOT_FRESH_DAEMON", "1"], [launch[i:i + 3] for i in range(len(launch))])
            self.assertNotIn("--unsetenv", launch)
            for call in state.calls[:3]:
                self.assertNotIn("CAGENTS_HOT_FRESH_DAEMON", call[1])

    def test_log_capture_preserves_hot_exit_status(self):
        with self.orchestration([]) as state:
            logfile = state.root / "hot output.log"
            with mock.patch.object(sys, "argv", [str(source), "--log-file", str(logfile), "--", "--help"]):
                self.assertEqual(hot.main(), 23)
            state.execute.assert_not_called()
            state.run.assert_called_once()
            self.assertEqual(state.run.call_args.args[0][-2:], ["--", "--help"])
            self.assertEqual(state.run.call_args.kwargs["stderr"], subprocess.STDOUT)
            output = state.run.call_args.kwargs["stdout"]
            self.assertEqual(output.name, str(logfile))
            self.assertTrue(output.closed)
            self.assertTrue(logfile.exists())

    def test_staging_preserves_reader_of_previous_executable_inode(self):
        fixtures = source.parents[1] / ".worktrees"
        fixtures.mkdir(exist_ok=True)
        with tempfile.TemporaryDirectory(prefix="hot-stage-test-", dir=fixtures) as directory:
            root = Path(directory)
            original = root / "cagentsd"
            original.write_bytes(b"previous executable")
            product = root / "built-cagentsd"
            product.write_bytes(b"replacement executable")
            product.chmod(0o755)
            with original.open("rb") as reader:
                previous_inode = os.fstat(reader.fileno()).st_ino
                hot.stage_product(product, original)
                self.assertEqual(reader.read(), b"previous executable")
                self.assertNotEqual(previous_inode, original.stat().st_ino)
                self.assertEqual(original.read_bytes(), b"replacement executable")
                self.assertEqual(original.stat().st_mode & 0o777, 0o755)
            self.assertEqual(list(root.glob(".hot-product-*")), [])

    def test_exact_recorded_tool_source_required(self):
        package = f"cargo-hot {hot.HOT_VERSION} (git+https://github.com/hecrj/cargo-hot.git?rev={hot.HOT_REVISION}#{hot.HOT_REVISION})"
        self.assertTrue(hot.matches_install(package, {"bins": ["cargo-hot"]}))
        self.assertFalse(hot.matches_install(package.replace(hot.HOT_REVISION, "a" * 40), {"bins": ["cargo-hot"]}))
        self.assertFalse(hot.matches_install(package.replace("0.1.1", "0.2.0"), {"bins": ["cargo-hot"]}))
        self.assertFalse(hot.matches_install(package.replace("/cargo-hot.git", "/cargo-hot-evil.git"), {"bins": ["cargo-hot"]}))
        self.assertFalse(hot.matches_install(package, {"bins": ["unrelated"]}))

    @unittest.skipUnless(shutil.which("bwrap"), "bubblewrap not installed")
    def test_real_mount_refuses_inplace_and_atomic_lock_changes_but_edits_source(self):
        # Disk-backed temporary fixture, never /tmp RAM or real repository state.
        fixtures = source.parents[1] / ".worktrees"
        fixtures.mkdir(exist_ok=True)
        with tempfile.TemporaryDirectory(prefix="hot-lock-test-", dir=fixtures) as directory:
            root = Path(directory)
            lock = root / "Cargo.lock"
            lock.write_text("immutable lock\n")
            script = root / "probe.py"
            script.write_text("""
import errno,json,os
from pathlib import Path
lock=Path('Cargo.lock')
results={}
for name,operation in [('write',lambda:lock.write_text('bad')),('unlink',lock.unlink),('rename',lambda:Path('replacement').replace(lock))]:
    Path('replacement').write_text('bad')
    try: operation()
    except OSError as error: results[name]=error.errno
    else: raise RuntimeError(name+' unexpectedly changed lock')
Path('source.rs').write_text('editable source')
assert lock.read_text()=='immutable lock\\n'
print(json.dumps({'uid':os.getuid(),'errors':results,'offline':os.environ.get('CARGO_NET_OFFLINE')}))
""")
            original = hot.ROOT
            hot.ROOT = root
            try:
                result = subprocess.run(hot.sandbox(["python3", str(script)], offline=True), capture_output=True, text=True, timeout=10)
            finally:
                hot.ROOT = original
            self.assertEqual(result.returncode, 0, result.stderr)
            values = json.loads(result.stdout)
            self.assertEqual(values["uid"], os.getuid())
            self.assertEqual(values["offline"], "true")
            self.assertEqual(set(values["errors"]), {"write", "unlink", "rename"})
            self.assertEqual(values["errors"]["write"], errno.EROFS)
            self.assertEqual(values["errors"]["unlink"], errno.EBUSY)
            self.assertEqual(values["errors"]["rename"], errno.EBUSY)
            self.assertEqual(lock.read_text(), "immutable lock\n")
            self.assertEqual((root / "source.rs").read_text(), "editable source")


if __name__ == "__main__":
    unittest.main()
