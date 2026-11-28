#!/usr/bin/env python3
"""Exercise the real smoke wrapper at its sandbox boundary; no host installs."""
import json
import fcntl
import importlib.util
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
_custody_spec = importlib.util.spec_from_file_location("fixture_custody", ROOT / "tests/fixture_custody.py")
fixture_custody = importlib.util.module_from_spec(_custody_spec)
_custody_spec.loader.exec_module(fixture_custody)
TMP_DIR = os.environ.get("TMPDIR", "/var/tmp")
VERSION = "1.2.3"
BUILD_CACHE_ID = (
    "cagents-local-release-v1 ubuntu-22.04.5 amd64 rust-1.93.0 zig-0.15.2 "
    "242cd8898b33ea806ef5f13b1076ed7c76f9f989d18384452f7166692438ff1a\n"
)
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
EDITOR_ARTIFACTS = (ARTIFACTS[0], ARTIFACTS[2], ARTIFACTS[4])


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
            "artifact=sys.argv[-2]\n"
            "bundled=sys.argv[sys.argv.index('CAGENTS_SMOKE_BUNDLED_EDITOR')+1]=='1'\n"
            f"has_editor=artifact=={ARTIFACTS[4]!r} or bundled and artifact in {EDITOR_ARTIFACTS!r}\n"
            "if has_editor and artifact!=os.environ.get('SMOKE_CONTRACT_NO_DIGEST'):\n"
            "    scratch=next(sys.argv[i + 1] for i, arg in enumerate(sys.argv)\n"
            "                 if arg=='--bind' and sys.argv[i + 2]=='/tmp')\n"
            "    letter='b' if artifact==os.environ.get('SMOKE_CONTRACT_OTHER_EDITOR') else 'a'\n"
            "    pathlib.Path(scratch, 'ceditor.sha256').write_text(letter*64+'\\n')\n"
            "sys.exit(17 if artifact==os.environ.get('SMOKE_CONTRACT_FAIL_ARTIFACT') else 0)\n"
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
        self.notes = self.directory / "notes.md"
        self.notes.write_text("Fixture release notes\n")
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
        _, stderr = smoke.communicate(timeout=HANG_GUARD)
        self.assertNotEqual(smoke.returncode, 0, stderr)
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
            self.assertEqual(forwarded["CAGENTS_SMOKE_BUNDLED_EDITOR"], "1")
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

    def test_missing_editor_digest_fails_the_release_smoke(self):
        result = self.invoke(SMOKE_CONTRACT_NO_DIGEST=ARTIFACTS[2])
        self.assertNotEqual(result.returncode, 0)
        self.assertIn(f"{ARTIFACTS[2]} produced no valid editor digest", result.stderr)
        self.assertNotIn(f"Artifact smoke passed: {ARTIFACTS[2]}\n", result.stdout)
        self.assert_clean()

    def test_legacy_layout_smokes_all_artifacts_without_full_package_editor_digests(self):
        result = self.invoke(arguments=["--dist", str(self.dist), "--version", VERSION,
                                        "--rootfs", str(self.base), "--legacy-editor-layout"])
        self.assertEqual(result.returncode, 0, result.stderr)
        for artifact in ARTIFACTS:
            self.assertIn(f"Artifact smoke passed: {artifact}\n", result.stdout)
        for arguments in self.recorded():
            self.assertEqual(arguments[arguments.index("CAGENTS_SMOKE_BUNDLED_EDITOR") + 1], "0")
        self.assert_clean()

    def test_same_version_editor_with_different_bytes_fails_the_release_smoke(self):
        for artifact in EDITOR_ARTIFACTS[1:]:
            with self.subTest(artifact=artifact):
                result = self.invoke(SMOKE_CONTRACT_OTHER_EDITOR=artifact)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn(f"{artifact} ships different editor bytes from {ARTIFACTS[0]}", result.stderr)
                self.assertNotIn(f"Artifact smoke passed: {artifact}\n", result.stdout)
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
            BUILD, ["--ref", "HEAD", "--notes", str(self.notes), "--output", str(output)],
            CARGO_BUILD_JOBS="1",
            CAGENTS_RELEASE_CACHE=str(build_cache),
            CAGENTS_RELEASE_SMOKE_CACHE=str(smoke_cache),
        )

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("cache marker does not match this runtime", result.stderr)
        self.assertFalse(self.network_calls.exists())
        self.assertEqual(self.recorded(), [])
        self.assertEqual(list(output.iterdir()), [])

    def build_cache(self):
        cache = self.directory / "build-cache"
        cache.mkdir(mode=0o700)
        (cache / ".cagents-release-cache").write_text(BUILD_CACHE_ID)
        self.environment.update(CAGENTS_RELEASE_CACHE=str(cache), CARGO_BUILD_JOBS="1")
        return cache

    def test_release_source_cache_checks_out_only_the_requested_tree(self):
        cache = self.build_cache()
        source = self.directory / "repository"
        checkout = cache / "source"
        checkout.mkdir()
        source.mkdir()

        def git(*args):
            return subprocess.check_output(
                ["git", "-C", str(source), "-c", "user.name=fixture",
                 "-c", "user.email=fixture@example.invalid", *args],
                text=True, timeout=HANG_GUARD,
            ).strip()

        def materialize(repository, destination, commit):
            subprocess.run(
                ["bash", "-ec", 'source "$1"; CACHE=$2 checkout_release_source "$3" "$4"',
                 "checkout", str(ROOT / "scripts/release-build-cache"),
                 str(destination), str(repository), commit],
                check=True, timeout=HANG_GUARD,
            )

        git("init", "-q")
        (source / "unchanged").write_text("shared file\n")
        (source / "changed").write_text("before\n")
        git("add", ".")
        git("commit", "-qm", "first tree")
        before = git("rev-parse", "HEAD")
        (source / "changed").write_text("after\n")
        git("commit", "-qam", "second tree")
        after = git("rev-parse", "HEAD")

        materialize(source, cache, after)
        self.assertEqual((checkout / "changed").read_text(), "after\n")
        self.assertFalse((checkout / ".git").exists())
        exported_history = subprocess.check_output(
            ["git", "--git-dir", str(cache / "source.git"), "rev-list", "--count", "HEAD"],
            text=True, timeout=HANG_GUARD,
        )
        self.assertEqual(exported_history.strip(), "1")
        unchanged = checkout / "unchanged"
        unchanged_mtime = unchanged.stat().st_mtime_ns
        (checkout / "untracked").write_text("left by the build\n")

        materialize(source, cache, before)
        self.assertEqual((checkout / "changed").read_text(), "before\n")
        self.assertEqual(unchanged.stat().st_mtime_ns, unchanged_mtime)
        self.assertFalse((checkout / "untracked").exists())

        shallow = self.directory / "shallow"
        subprocess.run(
            ["git", "clone", "-q", "--depth=1", source.as_uri(), str(shallow)],
            check=True, timeout=HANG_GUARD,
        )
        shallow_cache = self.directory / "shallow-cache"
        (shallow_cache / "source").mkdir(parents=True)
        materialize(shallow, shallow_cache, after)
        self.assertEqual((shallow_cache / "source/changed").read_text(), "after\n")

    def test_release_rejects_shallow_history_for_automatic_notes(self):
        self.build_cache()
        git = self.bin / "git"
        system_git = shlex.quote(shutil.which("git"))
        git.write_text(
            '#!/bin/sh\n'
            'if [ "$3" = rev-parse ] && [ "$4" = --is-shallow-repository ]; then\n'
            '    printf "true\\n"\n'
            '    exit 0\n'
            'fi\n'
            f'exec {system_git} "$@"\n'
        )
        git.chmod(0o755)
        output = self.directory / "out"
        result = self.invoke(BUILD, ["--ref", "HEAD", "--output", str(output)])
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("automatic release notes need full history", result.stderr)
        self.assertFalse(output.exists())
        self.assertFalse(self.network_calls.exists())
        self.assertEqual(self.recorded(), [])

    def test_release_builder_reclaims_only_recognized_scratch_in_its_marked_cache(self):
        cache = self.build_cache()
        abandoned = cache / "run.ABC123"
        (abandoned / "build-tmp").mkdir(parents=True, mode=0o700)
        abandoned.chmod(0o700)
        (abandoned / "build-tmp/link").symlink_to(self.base, target_is_directory=True)
        (cache / "run.DEF456").mkdir(mode=0o700)  # Interrupted just after mktemp.
        unrelated = cache / "run.GHI789"
        unrelated.mkdir(mode=0o700)
        (unrelated / ".unknown").write_text("unrelated")
        link = cache / "run.JKL012"
        link.symlink_to(self.base, target_is_directory=True)
        other = cache / "run.manual"
        other.write_text("user data")
        pinned = cache / "ubuntu-base.tar.gz"
        pinned.write_text("pinned archive")
        for directory in ("rootfs", "cargo", "target", "source", "source.git"):
            (cache / directory).mkdir()
        smoke_cache = self.directory / "wrong-smoke-cache"
        smoke_cache.mkdir(mode=0o700)
        (smoke_cache / ".cagents-release-smoke-cache").write_text("unexpected marker")
        self.environment["CAGENTS_RELEASE_SMOKE_CACHE"] = str(smoke_cache)
        published = self.directory / "published"
        published.mkdir()
        (published / "archive").write_text("verified asset")
        command = ["--ref", "HEAD", "--notes", str(self.notes), "--output", str(self.directory / "out")]

        with (cache / ".lock").open("w") as lock:
            fcntl.flock(lock, fcntl.LOCK_EX)
            result = self.invoke(BUILD, command)
            self.assertIn("another release build", result.stderr)
            self.assertTrue(abandoned.is_dir())
        marker = cache / ".cagents-release-cache"
        marker.write_text("other cache format")
        self.assertIn("cache marker does not match this builder", self.invoke(BUILD, command).stderr)
        self.assertTrue(abandoned.is_dir())
        marker.write_text(BUILD_CACHE_ID)

        result = self.invoke(BUILD, command)

        self.assertIn("cache marker does not match this runtime", result.stderr)
        self.assertFalse(abandoned.exists())
        self.assertFalse((cache / "run.DEF456").exists())
        self.assertEqual((unrelated / ".unknown").read_text(), "unrelated")
        self.assertEqual(other.read_text(), "user data")
        self.assertTrue(link.is_symlink())
        self.assertEqual((self.base / "base-sentinel").read_text(), "untouched\n")
        self.assertEqual(pinned.read_text(), "pinned archive")
        for directory in ("rootfs", "cargo", "target", "source", "source.git"):
            self.assertTrue((cache / directory).is_dir())
        self.assertEqual((published / "archive").read_text(), "verified asset")
        self.assertFalse(self.network_calls.exists())

    def test_killed_build_group_keeps_cache_locked_while_sandbox_descendant_drains(self):
        self.addCleanup(fixture_custody.settle)
        cache = self.build_cache()
        (cache / "ubuntu-base.tar.gz").touch()
        (cache / "rootfs").mkdir()
        smoke_cache = self.directory / "smoke-cache"
        smoke_cache.mkdir(mode=0o700)
        marker = smoke_cache / ".cagents-release-smoke-cache"
        marker.write_text(SMOKE_CACHE_ID)
        (smoke_cache / "ubuntu-base.tar.gz").touch()
        (smoke_cache / "rootfs/opt").mkdir(parents=True)
        (smoke_cache / "rootfs/opt/.cagents-release-smoke-provisioned-v4").write_text(SMOKE_CACHE_ID)
        checksum = self.bin / "sha256sum"
        checksum.write_text("#!/bin/sh\nexit 0\n")
        checksum.chmod(0o755)
        # Simulate a sandbox child that leaves the launcher's session and
        # keeps using scratch after the launcher receives TERM. Neither
        # process inherits the cache lock from the supervisor.
        boundary = self.bin / "bwrap"
        boundary.write_text(f"#!{sys.executable}\n" + r"""
import fcntl
import os
from pathlib import Path
import signal
import sys

scratch = Path(next(sys.argv[i - 1] for i, arg in enumerate(sys.argv) if arg == "/tmp"))
if os.fork():
    signal.pause()
    sys.exit(1)
os.setsid()
assert not Path("/proc/self/fd/9").exists()
lock = open(scratch.parent.parent / ".lock", "rb")

def draining(*_):
    try:
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
    except BlockingIOError:
        assert (scratch / "live").is_file()
        print("locked through sandbox teardown", flush=True)
        sys.exit(0)
    print("cache unlocked before sandbox teardown", flush=True)
    sys.exit(1)

signal.signal(signal.SIGTERM, draining)
(scratch / "live").touch()
print("sandbox ready", flush=True)
signal.pause()
""")
        self.environment["CAGENTS_RELEASE_SMOKE_CACHE"] = str(smoke_cache)
        command = [str(BUILD), "--ref", "HEAD", "--notes", str(self.notes), "--output", str(self.directory / "out")]
        with subprocess.Popen(command, env=self.environment, stdout=subprocess.PIPE,
                              stderr=subprocess.STDOUT, text=True, start_new_session=True) as builder:
            try:
                deadline = time.monotonic() + HANG_GUARD
                while not tuple(cache.glob("run.*/provision-tmp/live")):
                    self.assertIsNone(builder.poll(), "builder exited before the sandbox started")
                    self.assertLess(time.monotonic(), deadline, "sandbox did not start")
                    time.sleep(0.05)
                (scratch,) = cache.glob("run.*")
                os.killpg(builder.pid, signal.SIGKILL)
                output, _ = builder.communicate(timeout=HANG_GUARD)
            finally:
                if builder.poll() is None:
                    os.killpg(builder.pid, signal.SIGKILL)
        self.assertEqual(builder.returncode, -signal.SIGKILL, output)
        self.assertIn("locked through sandbox teardown", output)
        self.assertTrue(scratch.is_dir())
        marker.write_text("wrong smoke cache marker")

        result = self.invoke(BUILD, command[1:])

        self.assertIn("cache marker does not match this runtime", result.stderr)
        self.assertFalse(scratch.exists())
        self.assertEqual(list((self.directory / "out").iterdir()), [])

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
