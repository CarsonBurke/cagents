#!/usr/bin/env python3
"""Exercise the real smoke wrapper at its sandbox boundary; no host installs."""
import json
import fcntl
import os
from pathlib import Path
import subprocess
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[1]
SMOKE = ROOT / "scripts/smoke-release-artifacts"
PREPARE = ROOT / "scripts/prepare-release-smoke-rootfs"
VERSION = "1.2.3"
ARTIFACTS = (
    f"cagents_{VERSION}_amd64.deb",
    f"cagents-browserless_{VERSION}_amd64.deb",
    f"cagents-{VERSION}-x86_64-unknown-linux-gnu.tar.gz",
    f"cagents-browserless-{VERSION}-x86_64-unknown-linux-gnu.tar.gz",
    f"ceditor-{VERSION}-x86_64-unknown-linux-gnu.tar.gz",
)


class SmokeBoundary(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="cagents-smoke-contract-", dir="/var/tmp")
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
            "count=len(p.read_text().splitlines())\n"
            "sys.exit(17 if count==int(os.environ.get('SMOKE_CONTRACT_FAIL_AT','0')) else 0)\n"
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
            capture_output=True, text=True, timeout=15,
        )

    def recorded(self):
        return [json.loads(line) for line in self.calls.read_text().splitlines()] if self.calls.exists() else []

    def assert_clean(self):
        self.assertEqual(list(self.work.iterdir()), [])
        self.assertEqual((self.base / "base-sentinel").read_text(), "untouched\n")
        self.assertFalse(self.network_calls.exists())

    def test_each_artifact_gets_private_offline_namespace_and_read_only_inputs(self):
        result = self.invoke()
        self.assertEqual(result.returncode, 0, result.stderr)
        calls = self.recorded()
        self.assertEqual(len(calls), len(ARTIFACTS))
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

    def test_failed_artifact_stops_the_sequence_and_cleans_only_disposable_state(self):
        result = self.invoke(SMOKE_CONTRACT_FAIL_AT="2")
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(len(self.recorded()), 2)
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
        old = f"cagents-release-smoke-v1 ubuntu-24.04.3 amd64 {digest}\n"
        marker.write_text(old)
        root = cache / "rootfs"
        root.mkdir()
        sentinel = root / "existing-data"
        sentinel.write_text("preserve\n")
        result = self.invoke(PREPARE, ["--cache", str(cache)])
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("cache marker does not match", result.stderr)
        self.assertEqual(marker.read_text(), old)
        self.assertEqual(sentinel.read_text(), "preserve\n")
        self.assertEqual(self.recorded(), [])
        self.assertFalse(self.network_calls.exists())

    def test_preparation_respects_existing_cache_lock_before_fetching(self):
        cache = self.directory / "locked-cache"
        cache.mkdir(mode=0o700)
        digest = "6bc2cde3930ad088b3bb46fa45279e96d25bc3810f209850ecbe4722711874f9"
        (cache / ".cagents-release-smoke-cache").write_text(
            f"cagents-release-smoke-v2 ubuntu-24.04.3 amd64 {digest}\n"
        )
        with (cache / ".lock").open("w") as held:
            fcntl.flock(held, fcntl.LOCK_EX | fcntl.LOCK_NB)
            result = self.invoke(PREPARE, ["--cache", str(cache)])
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("another preparation", result.stderr)
        self.assertEqual(self.recorded(), [])
        self.assertFalse(self.network_calls.exists())


if __name__ == "__main__":
    unittest.main()
