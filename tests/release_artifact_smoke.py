#!/usr/bin/env python3
"""Exercise the real smoke wrapper at its sandbox boundary; no host installs."""
import json
import fcntl
import os
from pathlib import Path
import select
import shlex
import shutil
import signal
import socket
import subprocess
import sys
import tempfile
import time
import unittest


ROOT = Path(__file__).resolve().parents[1]
BUILD = ROOT / "scripts/build-release-linux"
SMOKE = ROOT / "scripts/smoke-release-artifacts"
PREPARE = ROOT / "scripts/prepare-release-smoke-rootfs"
ENTRYPOINT = ROOT / "packaging/smoke-release-artifact"
TMP_DIR = os.environ.get("TMPDIR", "/var/tmp")
VERSION = "1.2.3"
SMOKE_CACHE_ID = (
    "cagents-release-smoke-v4 ubuntu-24.04.3 amd64 "
    "6bc2cde3930ad088b3bb46fa45279e96d25bc3810f209850ecbe4722711874f9\n"
)
# Bounds only a hang; a passing wait returns as soon as its event arrives.
HANG_GUARD = 60
ARTIFACTS = (
    f"cagents_{VERSION}_amd64.deb",
    f"cagents-browserless_{VERSION}_amd64.deb",
    f"cagents-{VERSION}-x86_64-unknown-linux-gnu.tar.gz",
    f"cagents-browserless-{VERSION}-x86_64-unknown-linux-gnu.tar.gz",
    f"ceditor-{VERSION}-x86_64-unknown-linux-gnu.tar.gz",
)


class SmokeBoundary(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="cagents-smoke-contract-", dir=TMP_DIR)
        self.addCleanup(self.temp.cleanup)
        self.directory = Path(self.temp.name)
        self.bin = self.directory / "bin"
        self.bin.mkdir()
        self.calls = self.directory / "calls.jsonl"
        self.network_calls = self.directory / "network-called"
        boundary = self.bin / "bwrap"
        boundary.write_text(
            "#!/usr/bin/env python3\n"
            "import json,os,pathlib,sys\n"
            "p=pathlib.Path(os.environ['SMOKE_CONTRACT_CALLS'])\n"
            "with p.open('a') as output: output.write(json.dumps(sys.argv[1:])+'\\n')\n"
            "if os.environ.get('SMOKE_CONTRACT_HANG'):\n"
            "    pathlib.Path(os.environ['SMOKE_CONTRACT_HANG'], str(os.getpid())).touch()\n"
            "    import time; time.sleep(600)\n"
            "sys.exit(17 if sys.argv[-2]==os.environ.get('SMOKE_CONTRACT_FAIL_ARTIFACT') else 0)\n"
        )
        boundary.chmod(0o755)
        downloader = self.bin / "curl"
        downloader.write_text('#!/bin/sh\nprintf "unexpected network\\n" > "$SMOKE_CONTRACT_NETWORK"\nexit 99\n')
        downloader.chmod(0o755)
        self.base = self.directory / "rootfs"
        (self.base / "usr/bin").mkdir(parents=True)
        for name in ("dpkg", "jq"):
            executable = self.base / "usr/bin" / name
            executable.write_text("#!/bin/sh\nexit 0\n")
            executable.chmod(0o755)
        (self.base / "etc").mkdir()
        (self.base / "etc/os-release").write_text('ID=ubuntu\nVERSION_ID="24.04"\n')
        (self.base / "base-sentinel").write_text("untouched\n")
        self.dist = self.directory / "dist"
        self.dist.mkdir()
        for artifact in ARTIFACTS:
            (self.dist / artifact).write_text("boundary fixture only\n")
        self.work = self.directory / "work"
        self.work.mkdir()
        self.environment = os.environ.copy()
        self.environment.update(
            PATH=f"{self.bin}:{os.environ['PATH']}",
            SMOKE_CONTRACT_CALLS=str(self.calls),
            SMOKE_CONTRACT_NETWORK=str(self.network_calls),
            CAGENTS_SMOKE_WORK_DIR=str(self.work),
            # These must never become sandbox --setenv values or host mounts.
            CAGENTS_AGENT_CAPABILITY="not-for-the-artifact",
            GITHUB_TOKEN="not-for-the-artifact",
            SSH_AUTH_SOCK=str(self.directory / "host-agent.sock"),
        )

    def invoke(self, script=SMOKE, arguments=None, **environment):
        if arguments is None:
            arguments = ["--dist", str(self.dist), "--version", VERSION, "--rootfs", str(self.base)]
        return subprocess.run(
            [str(script), *arguments], env=self.environment | environment,
            capture_output=True, text=True, timeout=HANG_GUARD,
        )

    def recorded(self):
        return [json.loads(line) for line in self.calls.read_text().splitlines()] if self.calls.exists() else []

    def assert_clean(self):
        self.assertEqual(list(self.work.iterdir()), [])
        self.assertEqual((self.base / "base-sentinel").read_text(), "untouched\n")
        self.assertFalse(self.network_calls.exists())

    def test_an_interrupted_smoke_stops_every_run_before_removing_their_roots(self):
        running = self.directory / "running"
        running.mkdir()
        smoke = subprocess.Popen(
            [str(SMOKE), "--dist", str(self.dist), "--version", VERSION, "--rootfs", str(self.base)],
            env=self.environment | {"SMOKE_CONTRACT_HANG": str(running)},
            stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, text=True)
        deadline = time.monotonic() + HANG_GUARD
        while len(list(running.iterdir())) < len(ARTIFACTS):
            self.assertLess(time.monotonic(), deadline, "the smokes did not all start")
            time.sleep(0.05)
        smoke.send_signal(signal.SIGTERM)
        self.assertNotEqual(smoke.wait(timeout=HANG_GUARD), 0)
        for pid in (int(path.name) for path in running.iterdir()):
            try:
                os.kill(pid, 0)
            except ProcessLookupError:
                continue
            self.fail(f"sandbox {pid} outlived the interrupted smoke")
        self.assert_clean()

    def test_each_artifact_gets_private_offline_namespace_and_read_only_inputs(self):
        result = self.invoke()
        self.assertEqual(result.returncode, 0, result.stderr)
        # The artifacts run at once, so their calls arrive in any order.
        calls = sorted(self.recorded(), key=lambda arguments: ARTIFACTS.index(arguments[-2]))
        self.assertEqual([arguments[-2] for arguments in calls], list(ARTIFACTS))
        for arguments, artifact in zip(calls, ARTIFACTS):
            for flag in ("--unshare-all", "--clearenv", "--die-with-parent", "--new-session"):
                self.assertIn(flag, arguments)
            self.assertEqual(arguments[arguments.index("--uid") + 1], "0")
            self.assertEqual(arguments[arguments.index("--gid") + 1], "0")
            binds = [(arguments[i], arguments[i + 1], arguments[i + 2])
                     for i, argument in enumerate(arguments) if argument in ("--bind", "--ro-bind")]
            self.assertEqual(len(binds), 4)
            writable, scratch, frozen_artifact, frozen_script = binds
            self.assertEqual(writable[0], "--bind")
            self.assertEqual(writable[2], "/")
            self.assertTrue(Path(writable[1]).is_relative_to(self.work))
            self.assertNotEqual(Path(writable[1]), self.base)
            self.assertEqual(scratch[0], "--bind")
            self.assertEqual(scratch[2], "/tmp")
            self.assertTrue(Path(scratch[1]).is_relative_to(self.work))
            self.assertNotEqual(Path(scratch[1]), Path(writable[1]))
            self.assertEqual(frozen_artifact, ("--ro-bind", str(self.dist / artifact), "/artifact"))
            self.assertEqual(frozen_script[0], "--ro-bind")
            self.assertTrue(Path(frozen_script[1]).is_relative_to(self.work))
            self.assertEqual(frozen_script[2], "/smoke")
            private = [arguments[i + 1] for i, argument in enumerate(arguments) if argument == "--tmpfs"]
            self.assertCountEqual(private, ["/run", "/root", "/home"])
            self.assertEqual(arguments[arguments.index("--proc") + 1], "/proc")
            forwarded = {arguments[i + 1]: arguments[i + 2]
                         for i, argument in enumerate(arguments) if argument == "--setenv"}
            self.assertNotIn("GITHUB_TOKEN", forwarded)
            self.assertNotIn("SSH_AUTH_SOCK", forwarded)
            self.assertNotIn("CAGENTS_AGENT_CAPABILITY", forwarded)
            self.assertEqual(forwarded["XDG_RUNTIME_DIR"], "/run/fixture")
            self.assertEqual(arguments[-4:], ["/bin/bash", "/smoke", artifact, VERSION])
        self.assert_clean()

    def test_a_failed_artifact_fails_the_smoke_and_cleans_only_disposable_state(self):
        result = self.invoke(SMOKE_CONTRACT_FAIL_ARTIFACT=ARTIFACTS[1])
        self.assertNotEqual(result.returncode, 0)
        self.assertIn(f"{ARTIFACTS[1]} failed (exit 17)", result.stderr)
        self.assertNotIn(f"Artifact smoke passed: {ARTIFACTS[1]}\n", result.stdout)
        for artifact in (*ARTIFACTS[:1], *ARTIFACTS[2:]):
            self.assertIn(f"Artifact smoke passed: {artifact}\n", result.stdout)
        self.assert_clean()

    def test_missing_and_symlink_artifacts_reject_before_any_execution(self):
        artifact = self.dist / ARTIFACTS[-1]
        artifact.unlink()
        result = self.invoke()
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(self.recorded(), [])
        artifact.symlink_to(self.base / "base-sentinel")
        result = self.invoke()
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(self.recorded(), [])
        self.assert_clean()

    def test_wrong_distribution_rootfs_rejects_before_execution(self):
        for release in ('ID=debian\nVERSION_ID="24.04"\n', 'ID=ubuntu\nVERSION_ID="22.04"\n'):
            with self.subTest(release=release):
                (self.base / "etc/os-release").write_text(release)
                self.assertNotEqual(self.invoke().returncode, 0)
                self.assertEqual(self.recorded(), [])
        self.assert_clean()

    def test_symlink_rootfs_rejects_before_execution(self):
        self.assertEqual(
            (self.base / "etc/os-release").read_text(),
            'ID=ubuntu\nVERSION_ID="24.04"\n',
        )
        link = self.directory / "rootfs-link"
        link.symlink_to(self.base)
        result = self.invoke(arguments=["--dist", str(self.dist), "--version", VERSION,
                                        "--rootfs", str(link)])
        self.assertNotEqual(result.returncode, 0)
        self.assertIn(
            "missing rootfs; run scripts/prepare-release-smoke-rootfs or provide --rootfs",
            result.stderr,
        )
        self.assertEqual(self.recorded(), [])
        self.assert_clean()

    def test_preparation_refuses_unsafe_cache_before_network_or_namespace(self):
        cache = self.directory / "cache"
        cache.mkdir(mode=0o700)
        unexpected = cache / "unrelated"
        unexpected.write_text("preserve me\n")
        result = self.invoke(PREPARE, ["--cache", str(cache)])
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("unmarked nonempty", result.stderr)
        self.assertEqual(unexpected.read_text(), "preserve me\n")
        self.assertEqual(self.recorded(), [])
        unexpected.unlink()
        for entry in (".lock", ".cagents-release-smoke-cache", "ubuntu-base.tar.gz"):
            with self.subTest(entry=entry):
                link = cache / entry
                link.symlink_to(self.base / "base-sentinel")
                self.assertNotEqual(self.invoke(PREPARE, ["--cache", str(cache)]).returncode, 0)
                self.assertEqual(self.recorded(), [])
                link.unlink()
        cache.chmod(0o755)
        self.assertNotEqual(self.invoke(PREPARE, ["--cache", str(cache)]).returncode, 0)
        self.assertEqual(self.recorded(), [])
        self.assertFalse(self.network_calls.exists())

    def test_previous_runtime_cache_is_rejected_before_fetch_or_execution(self):
        cache = self.directory / "old-runtime-cache"
        cache.mkdir(mode=0o700)
        digest = "6bc2cde3930ad088b3bb46fa45279e96d25bc3810f209850ecbe4722711874f9"
        marker = cache / ".cagents-release-smoke-cache"
        old = f"cagents-release-smoke-v3 ubuntu-24.04.3 amd64 {digest}\n"
        marker.write_text(old)
        root = cache / "rootfs"
        root.mkdir()
        (root / "opt").mkdir()
        provision_marker = root / "opt/.cagents-release-smoke-provisioned-v3"
        provision_marker.write_text(old)
        sentinel = root / "existing-data"
        sentinel.write_text("preserve\n")
        result = self.invoke(PREPARE, ["--cache", str(cache)])
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("cache marker does not match", result.stderr)
        self.assertEqual(marker.read_text(), old)
        self.assertEqual(provision_marker.read_text(), old)
        self.assertEqual(sentinel.read_text(), "preserve\n")
        self.assertEqual(self.recorded(), [])
        self.assertFalse(self.network_calls.exists())

    def test_release_checks_runtime_cache_before_starting_the_build(self):
        smoke_cache = self.directory / "old-runtime-cache"
        smoke_cache.mkdir(mode=0o700)
        marker = smoke_cache / ".cagents-release-smoke-cache"
        marker.write_text(SMOKE_CACHE_ID.replace("v4", "v3", 1))
        build_cache = self.directory / "build-cache"
        output = self.directory / "release-output"

        result = self.invoke(
            BUILD, ["--ref", "HEAD", "--output", str(output)],
            CARGO_BUILD_JOBS="1",
            CAGENTS_RELEASE_CACHE=str(build_cache),
            CAGENTS_RELEASE_SMOKE_CACHE=str(smoke_cache),
        )

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("cache marker does not match this runtime", result.stderr)
        self.assertFalse(self.network_calls.exists())
        self.assertEqual(self.recorded(), [])
        self.assertEqual(list(output.iterdir()), [])

    def test_preparation_respects_existing_cache_lock_before_fetching(self):
        cache = self.directory / "locked-cache"
        cache.mkdir(mode=0o700)
        (cache / ".cagents-release-smoke-cache").write_text(SMOKE_CACHE_ID)
        running = cache / "prepare.ABC123"
        running.mkdir(mode=0o700)
        partial = running / "ubuntu-base.tar.gz"
        partial.write_text("partial download")
        with (cache / ".lock").open("w") as held:
            fcntl.flock(held, fcntl.LOCK_EX | fcntl.LOCK_NB)
            result = self.invoke(PREPARE, ["--cache", str(cache)])
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("another preparation", result.stderr)
            self.assertEqual(partial.read_text(), "partial download")
        self.assertEqual(self.recorded(), [])
        self.assertFalse(self.network_calls.exists())

    def test_preparation_reclaims_only_abandoned_scratch_trees(self):
        cache = self.directory / "interrupted-cache"
        cache.mkdir(mode=0o700)
        (cache / ".cagents-release-smoke-cache").write_text(SMOKE_CACHE_ID)
        abandoned = cache / "prepare.ABC123"
        abandoned.mkdir(mode=0o700)
        (abandoned / "ubuntu-base.tar.gz").write_text("partial download")
        (abandoned / "rootfs").mkdir()
        (abandoned / "rootfs/fixture-link").symlink_to(self.base / "base-sentinel")
        current = cache / "rootfs"
        current.mkdir()
        (current / "current-image").write_text("published image")
        unknown = cache / "prepare.DEF456"
        unknown.mkdir(mode=0o700)
        (unknown / ".unrecognized").write_text("preserve me")
        link = cache / "prepare.GHI789"
        link.symlink_to(self.base, target_is_directory=True)

        result = self.invoke(PREPARE, ["--cache", str(cache)])

        # The test download stub terminates preparation after the sweep.
        self.assertEqual(result.returncode, 99, result.stderr)
        self.assertTrue(self.network_calls.exists())
        self.assertFalse(abandoned.exists())
        self.assertEqual((current / "current-image").read_text(), "published image")
        self.assertEqual((unknown / ".unrecognized").read_text(), "preserve me")
        self.assertTrue(link.is_symlink())
        self.assertEqual((self.base / "base-sentinel").read_text(), "untouched\n")


@unittest.skipUnless(
    sys.platform == "linux" and hasattr(os, "pidfd_open") and hasattr(signal, "pidfd_send_signal"),
    "Linux pidfds required",
)
class ShutdownOrdering(unittest.TestCase):
    def check_shutdown(self, mode):
        self.assertIsNotNone(shutil.which("jq"), "jq is required for the real smoke predicates")
        real_readlink = shutil.which("readlink")
        self.assertIsNotNone(real_readlink, "readlink is required for the real identity guard")
        source = ENTRYPOINT.read_text()
        # Exercise the actual identity guard and shutdown stanza. Only redirect
        # their /tmp receipts into this test's private directory.
        guard = source[source.index("    same_daemon() {"):source.index('    "$daemon_cli" --json terminal list')]
        start = source.index('    "$daemon_cli" --json daemon stop-for-rebuild --including-subagents > /tmp/stop')
        shutdown = source[start:source.index("    daemon_cli=\n", start)]
        with tempfile.TemporaryDirectory(prefix="smoke-stop-", dir=TMP_DIR) as temporary:
            directory = Path(temporary)
            endpoint = directory / "socket"
            acknowledged = directory / "stopping"
            observed = directory / "identity-observed"
            premature = directory / "premature-status"
            processes = []

            def owned_process(*args, **kwargs):
                process = subprocess.Popen(*args, **kwargs)
                processes.append((process, None))
                processes[-1] = (process, os.pidfd_open(process.pid))
                return process

            try:
                daemon = owned_process(
                    [sys.executable, "-c",
                     "import signal,socket,sys\nfrom pathlib import Path\n"
                     "listener=socket.socket(socket.AF_UNIX)\nlistener.bind(sys.argv[1])\nlistener.listen(8)\n"
                     "signal.signal(signal.SIGTERM,lambda *_:Path(sys.argv[2]).write_text('stopping'))\n"
                     "print('ready',flush=True)\nsys.stdin.readline()\nlistener.close()\n",
                     str(endpoint), str(acknowledged)],
                    stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
                )
                self.assertTrue(select.select([daemon.stdout], [], [], HANG_GUARD)[0], "fixture listener did not start")
                self.assertEqual(daemon.stdout.readline(), "ready\n")
                epoch = Path(f"/proc/{daemon.pid}/stat").read_text().rsplit(")", 1)[1].split()[19]
                executable = os.readlink(f"/proc/{daemon.pid}/exe")
                tools = directory / "bin"
                tools.mkdir()
                readlink = tools / "readlink"
                readlink.write_text(
                    '#!/bin/sh\nvalue=$(' + shlex.quote(real_readlink) + ' "$@") || exit $?\n'
                    'printf "%s\\n" "$value"\n'
                    'if [ "$1" = "/proc/$SMOKE_PID/exe" ]; then : > "$SMOKE_OBSERVED"; fi\n'
                )
                readlink.chmod(0o755)
                cli = tools / "fixture-cli"
                cli.write_text(
                    f"#!{sys.executable}\n"
                    "import json,os,signal,socket,sys\nfrom pathlib import Path\n"
                    "if sys.argv[1:]==['--json','daemon','stop-for-rebuild','--including-subagents']:\n"
                    " p=int(os.environ['SMOKE_PID']);fd=os.pidfd_open(p)\n"
                    " assert Path(f'/proc/{p}/stat').read_text().rsplit(')',1)[1].split()[19]==os.environ['SMOKE_EPOCH']\n"
                    " signal.pidfd_send_signal(fd,signal.SIGTERM);os.close(fd)\n"
                    " print(json.dumps(dict(ok=True,command='daemon.stop_for_rebuild',data=dict(stopped=True))))\n"
                    "elif sys.argv[1:]==['--json','daemon','status']:\n"
                    " connection=socket.socket(socket.AF_UNIX);connection.settimeout(1)\n"
                    " try: connection.connect(os.environ['SMOKE_SOCKET'])\n"
                    " except (FileNotFoundError,ConnectionRefusedError): pass\n"
                    " else:\n"
                    "  Path(os.environ['SMOKE_PREMATURE']).write_text('status while original listener alive')\n"
                    "  print('protocol: Connection reset by peer (os error 104)',file=sys.stderr);sys.exit(1)\n"
                    " finally: connection.close()\n"
                    " if os.environ['SMOKE_MODE']=='error':\n"
                    "  print('unexpected status failure',file=sys.stderr);sys.exit(23)\n"
                    " print(json.dumps(dict(ok=True,command='daemon.status',data=dict(running=os.environ['SMOKE_MODE']=='replacement'))))\n"
                    "else: sys.exit(97)\n"
                )
                cli.chmod(0o755)
                environment = os.environ | dict(
                    PATH=f"{tools}:{os.environ['PATH']}", SMOKE_PID=str(daemon.pid), SMOKE_EPOCH=epoch,
                    SMOKE_EXECUTABLE=executable, SMOKE_CLI=str(cli), SMOKE_SOCKET=str(endpoint),
                    SMOKE_OBSERVED=str(observed), SMOKE_PREMATURE=str(premature), SMOKE_MODE=mode,
                )
                script = (
                    'set -euo pipefail\npid=$SMOKE_PID\nstart=$SMOKE_EPOCH\n'
                    'daemon_executable=$SMOKE_EXECUTABLE\ndaemon_cli=$SMOKE_CLI\n'
                    'fail() { printf "%s\\n" "$*" >&2; exit 1; }\n'
                    + guard + shutdown.replace("/tmp/status", shlex.quote(str(directory / "status")))
                    .replace("/tmp/stop", shlex.quote(str(directory / "stop")))
                )
                smoke = owned_process(
                    ["/bin/bash", "-c", script], env=environment,
                    stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
                )
                deadline = time.monotonic() + HANG_GUARD
                while not (acknowledged.exists() and observed.exists()):
                    if smoke.poll() is not None or time.monotonic() >= deadline:
                        self.assertFalse(premature.exists(), "status queried the original closing listener before the identity wait")
                        self.fail("shutdown must observe the real original identity before polling status")
                    time.sleep(0.01)
                self.assertIsNone(daemon.poll(), "fixture must remain alive until the identity wait")
                # This barrier, not a scheduling delay, holds the original socket
                # open until the production same_daemon wait has actually begun.
                with socket.socket(socket.AF_UNIX) as connection:
                    connection.settimeout(HANG_GUARD)
                    connection.connect(str(endpoint))
                daemon.stdin.write("exit\n")
                daemon.stdin.flush()
                daemon.wait(timeout=HANG_GUARD)
                stdout, stderr = smoke.communicate(timeout=HANG_GUARD)
                self.assertFalse(premature.exists(), "status queried the original closing listener")
                return smoke.returncode, stdout, stderr
            finally:
                for process, pidfd in reversed(processes):
                    try:
                        if process.poll() is None:
                            if pidfd is None:
                                # pidfd acquisition failed, but this direct child
                                # remains ours and unreaped, so its PID cannot recycle.
                                process.kill()
                            else:
                                signal.pidfd_send_signal(pidfd, signal.SIGKILL)
                        process.wait(timeout=HANG_GUARD)
                    finally:
                        if pidfd is not None:
                            os.close(pidfd)
                        for stream in (process.stdin, process.stdout, process.stderr):
                            if stream is not None:
                                stream.close()

    def test_async_stop_waits_for_original_process_before_status(self):
        code, stdout, stderr = self.check_shutdown("stopped")
        self.assertEqual(code, 0, stdout + stderr)

    def test_nonzero_status_failure_is_not_ignored(self):
        code, _, stderr = self.check_shutdown("error")
        self.assertEqual(code, 23, stderr)
        self.assertIn("unexpected status failure", stderr)

    def test_running_replacement_status_is_not_accepted_as_stopped(self):
        code, _, stderr = self.check_shutdown("replacement")
        self.assertNotEqual(code, 0, stderr)


if __name__ == "__main__":
    unittest.main()
