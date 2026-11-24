#!/usr/bin/env python3
"""Exercise `scripts/release-actions` against a private repository, real
ssh-keygen keys and a stand-in GitHub that records secrets and variables in
files. Nothing reaches GitHub."""

import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[1]
HANG_GUARD = 60

# Published releases, environment secrets and repository variables are files.
GH = r"""#!/bin/bash
echo "$*" >> "$FIXTURE/gh.log"
case "$1 $2" in
    "release list") cat "$FIXTURE/published" 2>/dev/null ;;
    "secret list") [[ ! -e $FIXTURE/list-fails ]] || exit 1; ls "$FIXTURE/secrets" 2>/dev/null ;;
    "secret set") mkdir -p "$FIXTURE/secrets"; cat > "$FIXTURE/secrets/$3" ;;
    "secret delete") rm -f "$FIXTURE/secrets/$3" ;;
    "variable set")
        mkdir -p "$FIXTURE/variables"
        name=$3
        while [[ $1 != --body ]]; do shift; done
        printf '%s' "$2" > "$FIXTURE/variables/$name" ;;
    *) echo "unexpected gh: $*" >&2; exit 99 ;;
esac
"""

# Checks the bundle as the real one would only by the fixture's say-so.
METADATA = r"""#!/bin/bash
echo "release-metadata $*" >> "$FIXTURE/calls.log"
[[ ! -e $FIXTURE/bundle-mismatch ]]
"""


class ReleaseActions(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory(prefix="cagents-release-actions-", dir="/var/tmp")
        self.addCleanup(self.directory.cleanup)
        self.root = Path(self.directory.name)
        self.repo = self.root / "repo"
        self.origin = self.root / "origin.git"
        self.bin = self.root / "bin"
        for directory in (self.repo / "scripts", self.repo / "src/update", self.bin, self.root / "scratch"):
            directory.mkdir(parents=True)
        self.tool(self.bin / "gh", GH)
        shutil.copy2(ROOT / "scripts/release-actions", self.repo / "scripts/release-actions")
        self.tool(self.repo / "scripts/release-metadata", METADATA)
        self.keys = {name: self.keygen(name) for name in ("security", "actions", "stranger")}

        self.git("init", "-q", "-b", "main")
        self.git("init", "-q", "--bare", str(self.origin), cwd=self.root)
        self.git("remote", "add", "origin", str(self.origin))
        self.release_commit("0.2.3", "security", generation=1)
        self.git("tag", "-a", "v0.2.3", "-m", "v0.2.3")
        self.published("v0.2.3")
        self.git("push", "-q", "origin", "main")

    def tool(self, path, text):
        path.write_text(text)
        path.chmod(0o755)

    def keygen(self, name):
        path = self.root / "keys" / name
        path.parent.mkdir(exist_ok=True)
        subprocess.run(["ssh-keygen", "-q", "-t", "ed25519", "-N", "", "-C", name, "-f", str(path)], check=True)
        return Path(f"{path}.pub").read_text().strip()

    def git(self, *args, cwd=None):
        return subprocess.run(["git", "-c", "user.name=Fixture", "-c", "user.email=fixture@example.invalid", *args],
                              cwd=cwd or self.repo, check=True, text=True, capture_output=True).stdout.strip()

    def release_commit(self, version, *trusted, generation=2):
        (self.repo / "Cargo.toml").write_text(f'[package]\nname = "cagents"\nversion = "{version}"\n')
        (self.repo / "src/update/feed.rs").write_text(
            f"const GENERATION: usize = {generation};\n" if generation > 1 else "// before generations\n")
        (self.repo / "src/update/release-keys.pub").write_text(
            "# Release keys.\n" + "".join(f"{self.keys[name]}\n" for name in trusted))
        self.git("add", "-A")
        self.git("commit", "-q", "-m", f"release {version}")
        return self.git("rev-parse", "HEAD")

    def published(self, *tags):
        (self.root / "published").write_text("".join(f"{tag}\n" for tag in tags))

    def run_actions(self, *args, actions="actions", trusted=("security", "actions"), env=None):
        environment = {
            **os.environ, "FIXTURE": str(self.root), "PATH": f"{self.bin}:{os.environ['PATH']}",
            "GITHUB_OUTPUT": str(self.root / "output"), "CAGENTS_SIGNING_SCRATCH": str(self.root / "scratch"),
            "RELEASE_SIGNING_PUBLIC_KEY": self.keys[actions] if actions else "",
            "RELEASE_TRUSTED_KEYS": "\n".join(self.keys[name] for name in trusted),
        }
        for name in ("GITHUB_SHA", "GITHUB_STEP_SUMMARY", "GIT_DIR", "GIT_WORK_TREE"):
            environment.pop(name, None)
        environment.update(env or {})
        (self.root / "output").unlink(missing_ok=True)
        return subprocess.run([str(self.repo / "scripts/release-actions"), *args], cwd=self.repo, env=environment,
                              text=True, capture_output=True, timeout=HANG_GUARD)

    def outputs(self):
        path = self.root / "output"
        return dict(line.split("=", 1) for line in path.read_text().splitlines()) if path.exists() else {}

    def tag(self, version, *trusted, generation=2):
        sha = self.release_commit(version, *trusted, generation=generation)
        self.git("tag", "-a", f"v{version}", "-m", f"v{version}")
        self.git("push", "-q", "origin", "main")
        return sha

    def test_the_release_after_one_that_does_not_trust_the_actions_key_is_signed_locally(self):
        sha = self.tag("0.2.4", "security", "actions")
        result = self.run_actions("admit", "v0.2.4", env={"GITHUB_SHA": sha})
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.outputs(), {"version": "0.2.4", "commit": sha, "sign": "false"})
        self.assertIn("v0.2.4 begins trust generation 2", result.stdout)

    def test_a_release_whose_predecessor_trusts_the_actions_key_is_signed_in_actions(self):
        self.tag("0.2.4", "security", "actions")
        self.published("v0.2.3", "v0.2.4")
        sha = self.tag("0.2.5", "security", "actions")
        result = self.run_actions("admit", "v0.2.5", env={"GITHUB_SHA": sha})
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.outputs(), {"version": "0.2.5", "commit": sha, "sign": "true"})

    def test_every_published_release_of_the_generation_must_trust_the_actions_key(self):
        # 0.2.5 adds the Actions key partway through generation 2; builds of
        # 0.2.4 still cannot verify a release signed by it alone.
        self.tag("0.2.4", "security")
        self.tag("0.2.5", "security", "actions")
        self.published("v0.2.3", "v0.2.4", "v0.2.5")
        self.tag("0.2.6", "security", "actions")
        result = self.run_actions("admit", "v0.2.6")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.outputs()["sign"], "false")
        self.assertIn("v0.2.4 does not trust the Actions key", result.stdout)

    def test_a_tag_never_published_is_no_ones_build(self):
        self.tag("0.2.4", "security")
        self.tag("0.2.5", "security", "actions")
        self.published("v0.2.3", "v0.2.5")
        self.tag("0.2.6", "security", "actions")
        result = self.run_actions("admit", "v0.2.6")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.outputs()["sign"], "true")
        # A published release missing here cannot be checked.
        self.git("tag", "-d", "v0.2.5")
        self.assertIn("the published v0.2.5 is not in v0.2.6's history here", self.run_actions("admit", "v0.2.6").stderr)

    def test_without_an_actions_key_every_release_is_signed_locally(self):
        self.tag("0.2.4", "security")
        result = self.run_actions("admit", "v0.2.4", actions=None)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.outputs()["sign"], "false")

    def test_a_change_of_trusted_keys_is_not_signed_in_actions(self):
        self.tag("0.2.4", "security", "actions")
        self.published("v0.2.3", "v0.2.4")
        self.tag("0.2.5", "security", "actions", "stranger")
        result = self.run_actions("admit", "v0.2.5")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("lists other keys than RELEASE_TRUSTED_KEYS", result.stderr)
        self.assertNotIn("sign", self.outputs())
        # A release that drops the Actions key is signed locally instead.
        self.git("tag", "-d", "v0.2.5")
        self.tag("0.2.6", "security")
        result = self.run_actions("admit", "v0.2.6")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.outputs()["sign"], "false")

    def test_a_release_that_begins_a_trust_generation_is_signed_locally(self):
        self.tag("0.2.4", "security", "actions")
        self.published("v0.2.3", "v0.2.4")
        self.tag("0.2.5", "security", "actions", generation=3)
        result = self.run_actions("admit", "v0.2.5")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.outputs()["sign"], "false")
        self.assertIn("v0.2.5 begins trust generation 3", result.stdout)

    def test_a_version_is_signed_once_and_only_after_every_published_one(self):
        self.tag("0.2.4", "security", "actions")
        self.published("v0.2.3", "v0.2.4")
        result = self.run_actions("admit", "v0.2.4")
        self.assertIn("published already", result.stderr)
        self.published("v0.2.3", "v0.3.0")
        result = self.run_actions("admit", "v0.2.4")
        self.assertIn("not newer than the published 0.3.0", result.stderr)
        self.assertEqual(self.outputs(), {})

    def test_a_tag_must_be_a_commit_on_main_that_declares_its_version(self):
        self.git("checkout", "-q", "-b", "side")
        self.release_commit("0.2.4", "security", "actions")
        self.git("tag", "-a", "v0.2.4", "-m", "v0.2.4")
        result = self.run_actions("admit", "v0.2.4")
        self.assertIn("not a commit on main", result.stderr)
        self.git("checkout", "-q", "main")
        self.git("tag", "-a", "v0.2.5", "-m", "v0.2.5")
        result = self.run_actions("admit", "v0.2.5")
        self.assertIn("Cargo.toml at v0.2.5 declares 0.2.3, not 0.2.5", result.stderr)
        sha = self.tag("0.2.6", "security", "actions")
        result = self.run_actions("admit", "v0.2.6", env={"GITHUB_SHA": "0" * 40})
        self.assertIn(f"v0.2.6 resolves to {sha}", result.stderr)
        self.assertIn("not a release tag", self.run_actions("admit", "v0.2.6-rc1").stderr)

    def test_sign_signs_the_checked_metadata_with_the_actions_key(self):
        dist = self.root / "dist"
        dist.mkdir()
        metadata = dist / "release-metadata.json"
        metadata.write_text('{"version":"0.2.5"}\n')
        key = (self.root / "keys/actions").read_text()
        result = self.run_actions("sign", str(dist), "0.2.5", "a" * 40, env={"RELEASE_SIGNING_KEY": key})
        self.assertEqual(result.returncode, 0, result.stderr)
        allowed = self.root / "allowed"
        allowed.write_text(f'cagents-release namespaces="cagents-release" {self.keys["actions"]}\n')
        with metadata.open("rb") as stdin:
            subprocess.run(["ssh-keygen", "-q", "-Y", "verify", "-f", str(allowed), "-I", "cagents-release",
                            "-n", "cagents-release", "-s", f"{metadata}.sig"], stdin=stdin, check=True,
                           capture_output=True)
        self.assertEqual(list((self.root / "scratch").iterdir()), [], "the key outlived the signing")
        self.assertIn(f"--version 0.2.5 --source-commit {'a' * 40}", (self.root / "calls.log").read_text())
        self.assertIn("signed already", self.run_actions("sign", str(dist), "0.2.5", "a" * 40,
                                                         env={"RELEASE_SIGNING_KEY": key}).stderr)

    def test_sign_refuses_a_bundle_that_does_not_match_or_a_key_that_is_not_the_public_one(self):
        dist = self.root / "dist"
        dist.mkdir()
        (dist / "release-metadata.json").write_text('{"version":"0.2.5"}\n')
        stranger = (self.root / "keys/stranger").read_text()
        result = self.run_actions("sign", str(dist), "0.2.5", "a" * 40, env={"RELEASE_SIGNING_KEY": stranger})
        self.assertIn("does not verify with RELEASE_SIGNING_PUBLIC_KEY", result.stderr)
        self.assertFalse((dist / "release-metadata.json.sig").exists())
        (self.root / "bundle-mismatch").touch()
        result = self.run_actions("sign", str(dist), "0.2.5", "a" * 40, env={"RELEASE_SIGNING_KEY": stranger})
        self.assertIn("does not match its metadata", result.stderr)
        self.assertFalse((dist / "release-metadata.json.sig").exists())
        self.assertEqual(list((self.root / "scratch").iterdir()), [])

    def test_keygen_stores_a_key_no_output_shows_and_refuses_to_replace_it(self):
        (self.root / "secrets").mkdir()
        (self.root / "secrets/RELEASE_KEYGEN_TOKEN").write_text("token")
        result = self.run_actions("keygen", actions=None, env={"GH_TOKEN": "token"})
        self.assertEqual(result.returncode, 0, result.stderr)
        private = (self.root / "secrets/RELEASE_SIGNING_KEY").read_text()
        self.assertIn("OPENSSH PRIVATE KEY", private)
        self.assertNotIn(private.splitlines()[1], result.stdout + result.stderr)
        self.assertFalse((self.root / "secrets/RELEASE_KEYGEN_TOKEN").exists())
        self.assertIn("--env release", (self.root / "gh.log").read_text())
        key = self.root / "stored"
        key.write_text(private)
        key.chmod(0o600)
        public = subprocess.run(["ssh-keygen", "-y", "-f", str(key)], check=True, text=True,
                                capture_output=True).stdout.split()[:2]
        self.assertEqual((self.root / "variables/RELEASE_SIGNING_PUBLIC_KEY").read_text(), " ".join(public))
        self.assertEqual((self.root / "variables/RELEASE_TRUSTED_KEYS").read_text().splitlines(),
                         sorted([" ".join(self.keys["security"].split()[:2]), " ".join(public)]))
        self.assertIn(f"{' '.join(public)} cagents-release-actions", result.stdout)
        self.assertEqual(list((self.root / "scratch").iterdir()), [])
        (self.root / "secrets/RELEASE_KEYGEN_TOKEN").write_text("token")
        result = self.run_actions("keygen", actions=None, env={"GH_TOKEN": "token"})
        self.assertIn("has a signing key already", result.stderr)
        self.assertEqual((self.root / "secrets/RELEASE_SIGNING_KEY").read_text(), private)
        # Refused or failed, the token does not outlive the run.
        self.assertFalse((self.root / "secrets/RELEASE_KEYGEN_TOKEN").exists())

    def test_keygen_stops_when_it_cannot_tell_whether_a_key_exists(self):
        (self.root / "secrets").mkdir()
        (self.root / "secrets/RELEASE_KEYGEN_TOKEN").write_text("token")
        (self.root / "list-fails").touch()
        result = self.run_actions("keygen", actions=None, env={"GH_TOKEN": "token"})
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("cannot list the release environment secrets", result.stderr)
        self.assertFalse((self.root / "secrets/RELEASE_SIGNING_KEY").exists())
        self.assertFalse((self.root / "secrets/RELEASE_KEYGEN_TOKEN").exists())
        self.assertIn("secret delete RELEASE_KEYGEN_TOKEN --env release-keygen", (self.root / "gh.log").read_text())


if __name__ == "__main__":
    unittest.main()
