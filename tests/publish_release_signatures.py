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


# GitHub for publication: releases are directories under $FAKE_GITHUB, tags
# are the checkout's own, and every request is logged.
GH = r"""#!/usr/bin/python3 -I
import json, os, shutil, subprocess, sys
from pathlib import Path

state = Path(os.environ["FAKE_GITHUB"])
checkout = os.environ["FAKE_CHECKOUT"]
arguments = sys.argv[1:]
with open(state / "requests.log", "a") as log:
    log.write(" ".join(arguments) + "\n")

def option(name):
    return arguments[arguments.index(name) + 1] if name in arguments else None

def releases():
    found = [json.loads((path / "release.json").read_text()) for path in sorted(state.glob("releases/*"))]
    for release in found:
        release["assets"] = [{"name": asset.name}
                             for asset in sorted((state / "releases" / release["tag_name"] / "assets").iterdir())]
    return found

def release(tag):
    return next(item for item in releases() if item["tag_name"] == tag)

def save(item):
    directory = state / "releases" / item["tag_name"]
    (directory / "assets").mkdir(parents=True, exist_ok=True)
    item = {key: value for key, value in item.items() if key != "assets"}
    (directory / "release.json").write_text(json.dumps(item))

def reply(value):
    text = json.dumps(value)
    if option("--jq"):
        text = subprocess.run(["jq", "-r", option("--jq")], input=text, text=True, capture_output=True,
                              check=True).stdout
    sys.stdout.write(text if text.endswith("\n") else text + "\n")

def git(*args):
    return subprocess.run(["git", "-C", checkout, *args], text=True, capture_output=True, check=True).stdout.strip()

if arguments[:2] == ["release", "download"]:
    directory = Path(option("--dir"))
    directory.mkdir(parents=True, exist_ok=True)
    patterns = [arguments[index + 1] for index, value in enumerate(arguments) if value == "--pattern"]
    assets = state / "releases" / arguments[2] / "assets"
    for asset in assets.iterdir():
        if not patterns or asset.name in patterns:
            shutil.copyfile(asset, directory / asset.name)
elif arguments[:2] == ["release", "upload"]:
    files = [value for value in arguments[3:] if value.startswith("/")]
    for path in files:
        shutil.copyfile(path, state / "releases" / arguments[2] / "assets" / Path(path).name)
elif arguments[0] == "api":
    path = next(value for value in arguments[1:] if value.startswith("repos/"))
    method = option("--method") or "GET"
    tail = path.split("/", 3)[3] if path.count("/") >= 3 else ""
    if tail == "":
        reply({"full_name": "CarsonBurke/cagents"})
    elif tail.startswith("git/ref/tags/"):
        tag = tail.removeprefix("git/ref/tags/")
        reply({"object": {"sha": git("rev-parse", f"refs/tags/{tag}"), "type": git("cat-file", "-t", f"refs/tags/{tag}")}})
    elif tail.startswith("git/tags/"):
        sha = tail.removeprefix("git/tags/")
        target = git("rev-parse", f"{sha}^{{}}")
        reply({"object": {"sha": target, "type": git("cat-file", "-t", target)}})
    elif tail.startswith("releases?"):
        if method == "POST":
            raise SystemExit("unexpected")
        reply(releases())
    elif tail == "releases" and method == "POST":
        fields = dict(value.split("=", 1) for value in arguments if "=" in value and not value.startswith("repos/"))
        item = {"id": 100 + len(releases()), "tag_name": fields["tag_name"], "draft": True,
                "body": Path(fields["body"].removeprefix("@")).read_text()}
        save(item)
        reply(release(item["tag_name"]))
    elif tail.startswith("releases/"):
        number = int(tail.removeprefix("releases/"))
        item = next(item for item in releases() if item["id"] == number)
        if method == "PATCH":
            item["draft"] = False
            item["html_url"] = f"https://github.com/CarsonBurke/cagents/releases/tag/{item['tag_name']}"
            save(item)
        reply(item)
    else:
        raise SystemExit(f"unexpected gh api {path}")
else:
    raise SystemExit(f"unexpected gh {arguments}")
"""


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
        self.keys = {name: self.keygen(name) for name in ("old", "new", "next", "stranger")}
        self.checkout = self.directory / "checkout"
        scripts = self.checkout / "scripts"
        scripts.mkdir(parents=True)
        for name in ("publish-release", "release-metadata"):
            shutil.copy2(ROOT / "scripts" / name, scripts / name)
        # Artifact smoke has its own suite; here it only must be reached.
        (scripts / "smoke-release-artifacts").write_text("#!/bin/sh\nexit 0\n")
        (scripts / "smoke-release-artifacts").chmod(0o755)
        self.git("init", "-q")
        self.release("0.2.3", ["old"], generation=1)
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

    def release(self, version: str, trusted: list[str], generation: int = 2):
        """Commits and tags a source of VERSION that trusts the TRUSTED keys,
        of trust GENERATION, which sources before generations do not name."""
        feed = self.checkout / "src/update/feed.rs"
        feed.parent.mkdir(parents=True, exist_ok=True)
        feed.write_text(f"const GENERATION: usize = {generation};\n" if generation > 1 else "// no generations\n")
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

    def bridges(self, **published: Path) -> Path:
        """A --bridge directory holding, under each published NAME, the
        signed metadata of the bundle given for it."""
        directory = Path(tempfile.mkdtemp(prefix="bridges-", dir=self.directory))
        for name, bundle in published.items():
            name = name.replace("_", "-") + ".json"
            shutil.copyfile(bundle / "release-metadata.json", directory / name)
            shutil.copyfile(bundle / "release-metadata.json.sig", directory / f"{name}.sig")
        return directory

    def published(self, version: str, bundle: Path, names: dict[str, str]):
        """Publishes BUNDLE's metadata as VERSION's release, under NAMES
        (published name: bundle name)."""
        github = self.directory / "github"
        assets = github / "releases" / f"v{version}" / "assets"
        assets.mkdir(parents=True)
        (assets.parent / "release.json").write_text(
            f'{{"id": {len(list(github.glob("releases/*")))}, "tag_name": "v{version}", "draft": false, "body": ""}}')
        for published, name in names.items():
            shutil.copyfile(bundle / name, assets / published)

    def publish(self, version: str, dist: Path, *extra: str) -> subprocess.CompletedProcess:
        tools = self.directory / "tools"
        tools.mkdir(exist_ok=True)
        (tools / "gh").write_text(GH)
        (tools / "gh").chmod(0o755)
        (self.directory / "github").mkdir(exist_ok=True)
        environment = dict(self.environment, PATH=f"{tools}:{os.environ['PATH']}",
                           FAKE_GITHUB=str(self.directory / "github"), FAKE_CHECKOUT=str(self.checkout))
        return subprocess.run([str(self.checkout / "scripts/publish-release"), "--version", version,
                               "--dist", str(dist), *extra], cwd=self.checkout, text=True, capture_output=True,
                              env=environment, timeout=HANG_GUARD)

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
        self.assertRefused(self.verify("0.2.5", release, "--bridge", str(release)), "is not v0.2.4's")

    def test_every_build_of_a_generation_must_verify_its_later_releases(self):
        # 0.2.6 trusts only the next key; builds of 0.2.4 and 0.2.5 do not.
        self.release("0.2.6", ["next"])
        bridge = self.bridges(release_metadata=self.bundle("0.2.4", ["old"]))
        self.assertRefused(self.verify("0.2.6", self.bundle("0.2.6", ["next"]), "--bridge", str(bridge)),
                           "not signed by a key v0.2.4 trusts")

    def test_a_new_generation_recovers_from_a_withdrawn_key_without_stranding_anyone(self):
        # The Actions key ("new") leaked: 0.2.6 drops it for "next" and begins
        # generation 3, signed by the security key every generation 2 build trusts.
        self.release("0.2.6", ["old", "next"], generation=3)
        self.release("0.2.7", ["old", "next"], generation=3)
        legacy = self.bundle("0.2.4", ["old"])
        older = self.bridges(release_metadata=legacy)
        self.assertRefused(self.verify("0.2.6", self.bundle("0.2.6", ["next"]), "--bridge", str(older)),
                           "not signed by a key v0.2.4 trusts")
        recovery = self.bundle("0.2.6", ["old"])
        result = self.verify("0.2.6", recovery, "--bridge", str(older))
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("Validated v0.2.6: 18 assets", result.stdout)
        # Later releases need only the new key: generation 2 builds read
        # 0.2.6's metadata, which they trust, and generation 1 builds 0.2.4's.
        both = self.bridges(release_metadata=legacy, release_metadata_2=recovery)
        result = self.verify("0.2.7", self.bundle("0.2.7", ["next"]), "--bridge", str(both))
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertRefused(self.verify("0.2.7", self.bundle("0.2.7", ["new"]), "--bridge", str(both)),
                           "the tagged source trusts")
        self.assertRefused(self.verify("0.2.7", self.bundle("0.2.7", ["next"]), "--bridge", str(older)),
                           "release-metadata-2.json")
        wrong = self.bridges(release_metadata=legacy, release_metadata_2=self.bundle("0.2.5", ["new"]))
        self.assertRefused(self.verify("0.2.7", self.bundle("0.2.7", ["next"]), "--bridge", str(wrong)),
                           "is not v0.2.6's")

    def test_a_generation_follows_the_one_before_it(self):
        self.release("0.2.6", ["old", "new"], generation=4)
        self.assertRefused(self.verify("0.2.6", self.bundle("0.2.6", ["new"])), "follows the one before")
        self.release("0.2.7", ["old", "new"], generation=1)
        self.assertRefused(self.verify("0.2.7", self.bundle("0.2.7", ["new"])), "follows the one before")

    def test_publication_carries_the_bridge_as_the_bridge_release_serves_it(self):
        bridge = self.bundle("0.2.4", ["old"])
        self.published("0.2.4", bridge, {"release-metadata.json": "release-metadata.json",
                                         "release-metadata.json.sig": "release-metadata.json.sig"})
        release = self.bundle("0.2.5", ["new"])
        # A supplied copy that is not what 0.2.4 serves is refused.
        result = self.publish("0.2.5", release, "--bridge", str(self.bundle("0.2.4", ["new"])))
        self.assertRefused(result, "differs from what v0.2.4 publishes")
        result = self.publish("0.2.5", release)
        self.assertEqual(result.returncode, 0, result.stderr)
        served = self.directory / "github/releases/v0.2.5/assets"
        self.assertEqual((served / "release-metadata.json.sig").read_bytes(),
                         (bridge / "release-metadata.json.sig").read_bytes())
        self.assertEqual((served / "release-metadata-2.json").read_bytes(),
                         (release / "release-metadata.json").read_bytes())
        self.assertIn('"draft": false', (self.directory / "github/releases/v0.2.5/release.json").read_text())

    def test_publication_never_puts_a_release_above_a_newer_one(self):
        bridge = self.bundle("0.2.4", ["old"])
        self.published("0.2.4", bridge, {"release-metadata.json": "release-metadata.json",
                                         "release-metadata.json.sig": "release-metadata.json.sig"})
        self.published("0.2.6", bridge, {})
        result = self.publish("0.2.5", self.bundle("0.2.5", ["new"]))
        self.assertRefused(result, "0.2.6 is published already")
        self.assertIn('"draft": true', (self.directory / "github/releases/v0.2.5/release.json").read_text())

    def test_a_signature_file_holds_only_signatures(self):
        dist = self.bundle("0.2.5", ["new"])
        signature = (dist / "release-metadata.json.sig").read_text()
        bridge = self.bundle("0.2.4", ["old"])
        for damaged in (signature + "trailing text\n", signature * 5, signature[:-30]):
            (dist / "release-metadata.json.sig").write_text(damaged)
            self.assertNotEqual(self.verify("0.2.5", dist, "--bridge", str(bridge)).returncode, 0)


if __name__ == "__main__":
    unittest.main()
