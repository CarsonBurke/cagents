#!/usr/bin/env python3
"""Exercise the publisher's signature and bridge checks on real bundles in
--verify-only mode, with real ssh-keygen keys; artifact smoke is stubbed and
nothing reaches the network."""
import io
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tarfile
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[1]
TARGET = "x86_64-unknown-linux-gnu"
# Bounds only a hang; a passing run returns as soon as its process exits.
HANG_GUARD = 120
MANIFESTS = ("Cargo.toml", "crates/cagents-ui/Cargo.toml", "crates/ceditor/Cargo.toml")


def archive(path: Path, root: str, files: dict[str, bytes], executables: set[str], links: dict[str, str]):
    """A gzipped tar of ROOT/ holding FILES, EXECUTABLES and symbolic LINKS,
    with every parent directory as its own member."""
    directories = {""}
    for name in [*files, *links]:
        parts = name.split("/")[:-1]
        for depth in range(1, len(parts) + 1):
            directories.add("/".join(parts[:depth]))
    with tarfile.open(path, "w:gz", format=tarfile.GNU_FORMAT) as output:
        for directory in sorted(directories):
            info = tarfile.TarInfo(f"{root}/{directory}".rstrip("/"))
            info.type, info.mode = tarfile.DIRTYPE, 0o755
            output.addfile(info)
        for name, data in files.items():
            info = tarfile.TarInfo(f"{root}/{name}")
            info.size, info.mode = len(data), 0o755 if name in executables else 0o644
            output.addfile(info, io.BytesIO(data))
        for name, target in links.items():
            info = tarfile.TarInfo(f"{root}/{name}")
            info.type, info.linkname, info.mode = tarfile.SYMTYPE, target, 0o777
            output.addfile(info)


def cagents_archive(path: Path, product: str, version: str, desktop: str):
    files = {name: b"x" for name in (
        "bin/cagents", "bin/cagents-ui", "bin/cagentsd", "bin/.cagents-ui-lease-v1",
        f"share/applications/{desktop}.desktop", f"share/autostart/{desktop}.Daemon.desktop",
        f"share/icons/hicolor/scalable/apps/{desktop}.svg",
        f"share/icons/hicolor/128x128/apps/{desktop}.png",
        f"share/metainfo/{desktop}.metainfo.xml", "share/doc/cagents/LICENSE")}
    executables = {"bin/cagents", "bin/cagents-ui", "bin/cagentsd"}
    links = {}
    if product == "cagents":
        files["bin/cagents-uninstall"] = b"x"
        executables.add("bin/cagents-uninstall")
    else:
        links["bin/cagents-browserless"] = "cagents"
    archive(path, f"{product}-{version}", files, executables, links)


def editor_archive(path: Path, version: str):
    release = f"lib/ceditor/releases/{version}/bin"
    files = {f"{release}/ceditor": b"x", f"{release}/cagents": b"x", f"{release}/cagentsd": b"x",
             f"{release}/.ceditor-companions": b"ceditor companions v1\n",
             "share/applications/io.github.cagents.ceditor.desktop": b"x",
             "share/icons/hicolor/scalable/apps/io.github.cagents.ceditor.svg": b"x",
             "share/doc/ceditor/LICENSE": b"x"}
    executables = {f"{release}/ceditor", f"{release}/cagents", f"{release}/cagentsd"}
    links = {"bin/ceditor": "../lib/ceditor/current/bin/ceditor",
             "lib/ceditor/current": f"releases/{version}"}
    archive(path, f"ceditor-{version}", files, executables, links)


class PublisherSignatures(unittest.TestCase):
    def setUp(self):
        for command in ("ssh-keygen", "git", "jq", "sha256sum"):
            if shutil.which(command) is None:
                self.skipTest(f"{command} is not installed")
        temporary = tempfile.TemporaryDirectory(prefix="publisher-signatures-")
        self.addCleanup(temporary.cleanup)
        self.directory = Path(temporary.name)
        self.environment = {key: value for key, value in os.environ.items() if not key.startswith("GIT_")}
        self.environment.update(
            HOME=str(self.directory / "home"), GIT_CONFIG_NOSYSTEM="1", GIT_CONFIG_GLOBAL=os.devnull,
            GIT_AUTHOR_NAME="t", GIT_AUTHOR_EMAIL="t@example.invalid",
            GIT_COMMITTER_NAME="t", GIT_COMMITTER_EMAIL="t@example.invalid",
            TMPDIR=str(self.directory), GITHUB_ACTIONS="",
        )
        (self.directory / "home").mkdir()
        self.keys = {name: self.keygen(name) for name in ("old", "new", "stranger")}
        self.checkout = self.directory / "checkout"
        scripts = self.checkout / "scripts"
        scripts.mkdir(parents=True)
        for name in ("publish-release", "release-metadata"):
            shutil.copy2(ROOT / "scripts" / name, scripts / name)
        # Artifact smoke has its own suite; here it only must be reached.
        (scripts / "smoke-release-artifacts").write_text("#!/bin/sh\nexit 0\n")
        (scripts / "smoke-release-artifacts").chmod(0o755)
        self.git("init", "-q")
        self.release("0.2.3", ["old"])
        self.release("0.2.4", ["old", "new"])
        self.release("0.2.5", ["old", "new"])

    def keygen(self, name: str) -> Path:
        key = self.directory / f"{name}-key"
        subprocess.run(["ssh-keygen", "-q", "-t", "ed25519", "-N", "", "-C", name, "-f", str(key)],
                       check=True, timeout=HANG_GUARD)
        return key

    def git(self, *args: str) -> str:
        return subprocess.run(["git", "-C", str(self.checkout), *args], check=True, text=True,
                              capture_output=True, env=self.environment, timeout=HANG_GUARD).stdout

    def release(self, version: str, trusted: list[str]):
        """Commits and tags a source of VERSION that trusts the TRUSTED keys."""
        for manifest in MANIFESTS:
            path = self.checkout / manifest
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(f'[package]\nname = "{path.parent.name}"\nversion = "{version}"\n')
        ipc = self.checkout / "src/terminal/ipc.rs"
        ipc.parent.mkdir(parents=True, exist_ok=True)
        ipc.write_text("pub const PROTOCOL_VERSION: u16 = 9;\n")
        keys = self.checkout / "src/update/release-keys.pub"
        keys.parent.mkdir(parents=True, exist_ok=True)
        keys.write_text("# trusted\n" + "".join(
            Path(f"{self.keys[name]}.pub").read_text()
            for name in trusted))
        self.git("add", "-A")
        self.git("commit", "-q", "-m", f"release {version}")
        self.git("tag", "-a", f"v{version}", "-m", f"v{version}")

    def bundle(self, version: str, signers: list[str]) -> Path:
        """A complete bundle of VERSION, its metadata signed by SIGNERS in order."""
        dist = Path(tempfile.mkdtemp(prefix=f"dist-{version}-", dir=self.directory))
        cagents_archive(dist / f"cagents-{version}-{TARGET}.tar.gz", "cagents", version, "dev.cagents.Cagents")
        cagents_archive(dist / f"cagents-browserless-{version}-{TARGET}.tar.gz", "cagents-browserless", version,
                        "dev.cagents.Cagents.Browserless")
        editor_archive(dist / f"ceditor-{version}-{TARGET}.tar.gz", version)
        for name in (f"cagents_{version}_amd64.deb", f"cagents-browserless_{version}_amd64.deb",
                     f"cagents-{version}-sizes.txt", f"cagents-{version}-artifact-comparison.txt",
                     f"cagents-browserless-{version}-sizes.txt"):
            (dist / name).write_text(name)
        (dist / "install.sh").write_text("#!/bin/sh\nDEFAULT_REPOSITORY='CarsonBurke/cagents'\n")

        def sums(manifest: str, names: list[str]):
            lines = subprocess.run(["sha256sum", "--", *names], cwd=dist, check=True, text=True,
                                   capture_output=True, timeout=HANG_GUARD).stdout
            (dist / manifest).write_text(lines)
        browserless = [f"cagents-browserless-{version}-{TARGET}.tar.gz", f"cagents-browserless_{version}_amd64.deb",
                       f"cagents-browserless-{version}-sizes.txt"]
        sums("SHA256SUMS-browserless", browserless)
        sums("SHA256SUMS-ceditor", [f"ceditor-{version}-{TARGET}.tar.gz"])
        sums("SHA256SUMS", sorted(path.name for path in dist.iterdir()))
        commit = self.git("rev-parse", f"v{version}^{{commit}}").strip()
        subprocess.run([sys.executable, "-I", str(self.checkout / "scripts/release-metadata"), "write",
                        "--dir", str(dist), "--version", version, "--source-commit", commit, "--protocol", "9"],
                       check=True, capture_output=True, timeout=HANG_GUARD)
        self.sign(dist, signers)
        return dist

    def sign(self, dist: Path, signers: list[str]):
        signature = ""
        for signer in signers:
            subprocess.run(["ssh-keygen", "-q", "-Y", "sign", "-n", "cagents-release", "-f", str(self.keys[signer]),
                            str(dist / "release-metadata.json")], check=True, capture_output=True,
                           timeout=HANG_GUARD)
            made = dist / "release-metadata.json.sig"
            signature += made.read_text()
            made.unlink()
        (dist / "release-metadata.json.sig").write_text(signature)

    def verify(self, version: str, dist: Path, *extra: str) -> subprocess.CompletedProcess:
        return subprocess.run([str(self.checkout / "scripts/publish-release"), "--version", version,
                               "--dist", str(dist), "--verify-only", *extra],
                              cwd=self.checkout, text=True, capture_output=True, env=self.environment,
                              timeout=HANG_GUARD)

    def assertRefused(self, result: subprocess.CompletedProcess, reason: str):
        self.assertNotEqual(result.returncode, 0, result.stdout)
        self.assertIn(reason, result.stderr)

    def test_the_bridge_release_is_signed_once_by_a_key_the_previous_release_trusts(self):
        result = self.verify("0.2.4", self.bundle("0.2.4", ["old"]))
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("Validated v0.2.4: 16 assets", result.stdout)
        # 0.2.3 reads exactly one signature, so a second one would strand it.
        self.assertRefused(self.verify("0.2.4", self.bundle("0.2.4", ["old", "new"])), "exactly one signature")
        # Signed by a key only 0.2.4 trusts, 0.2.3 could not install it.
        self.assertRefused(self.verify("0.2.4", self.bundle("0.2.4", ["new"])), "v0.2.3 trusts")

    def test_a_later_release_carries_the_bridge_and_may_hold_several_signatures(self):
        bridge = self.bundle("0.2.4", ["old"])
        self.assertEqual(self.verify("0.2.5", self.bundle("0.2.5", ["new"]), "--bridge", str(bridge)).returncode, 0)
        # One signature by a trusted key is enough; the stranger's is not needed.
        result = self.verify("0.2.5", self.bundle("0.2.5", ["stranger", "new"]), "--bridge", str(bridge))
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertRefused(self.verify("0.2.5", self.bundle("0.2.5", ["stranger"]), "--bridge", str(bridge)),
                           "not a signature by a key the tagged source trusts")
        # Offline verification cannot fetch the bridge, so it must be given.
        self.assertRefused(self.verify("0.2.5", self.bundle("0.2.5", ["new"])), "--bridge")

    def test_a_bridge_must_be_the_bridge_releases_metadata_signed_for_the_builds_before_it(self):
        release = self.bundle("0.2.5", ["new"])
        self.assertRefused(self.verify("0.2.5", release, "--bridge", str(self.bundle("0.2.4", ["new"]))),
                           "not signed by a key v0.2.3 trusts")
        self.assertRefused(self.verify("0.2.5", release, "--bridge", str(self.bundle("0.2.4", ["old", "old"]))),
                           "exactly one signature")
        self.assertRefused(self.verify("0.2.5", release, "--bridge", str(release)), "not 0.2.4's")

    def test_a_signature_file_holds_only_signatures(self):
        dist = self.bundle("0.2.5", ["new"])
        signature = (dist / "release-metadata.json.sig").read_text()
        bridge = self.bundle("0.2.4", ["old"])
        for damaged in (signature + "trailing text\n", signature * 5, signature[:-30]):
            (dist / "release-metadata.json.sig").write_text(damaged)
            self.assertNotEqual(self.verify("0.2.5", dist, "--bridge", str(bridge)).returncode, 0)


if __name__ == "__main__":
    unittest.main()
