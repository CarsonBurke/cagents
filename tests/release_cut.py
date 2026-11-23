#!/usr/bin/env python3
"""Exercise `scripts/release cut` and `publish` against a private repository,
a stand-in build host reached through stand-in ssh and scp, and stand-in
GitHub, signing and publishing tools. Nothing builds, signs or publishes."""

import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[1]
TARGET = "x86_64-unknown-linux-gnu"
HANG_GUARD = 120

SSH = r"""#!/bin/bash
# As ssh HOST bash -s -- ARGS: the script on standard input runs on the host.
# Like ssh, it joins the arguments for the remote shell to parse again.
host=$1; shift
[[ $1 == bash ]] || { echo "unexpected ssh: $*" >&2; exit 99; }
echo "$host $*" >> "$FIXTURE/ssh.log"
HOME=$FIXTURE/host exec bash -c "$*"
"""

SCP = r"""#!/bin/bash
args=()
for argument in "$@"; do
    case $argument in -q|-r|--) ;; *:*) args+=("$FIXTURE/host/${argument#*:}") ;; *) args+=("$argument") ;; esac
done
exec cp -r "${args[@]}"
"""

# The host's prebuild-release: its status is the fixture's, and what it is
# asked is recorded. wait bundles the requested commit from waiting/SHA.
PREBUILD = r"""#!/bin/bash
fixture=$FIXTURE
while [[ $1 == --* ]]; do shift 2; done
echo "$*" >> "$fixture/prebuild.log"
case $1 in
    status) cat "$fixture/status.json" ;;
    pin) [[ ! -e $fixture/pin-fails ]] ;;
    unpin|request) ;;
    wait)
        mv "$fixture/waiting/$2" "$HOME/.local/state/cagents-prebuilt/bundles/$2"
        echo "$2" ;;
esac
"""

SENTINEL = r"""#!/bin/bash
echo "$*" >> "$FIXTURE/sentinel.log"
exit "$(cat "$FIXTURE/sentinel-status" 2>/dev/null || echo 0)"
"""

GH = r"""#!/bin/bash
echo "$*" >> "$FIXTURE/gh.log"
case "$1 $2" in
    "pr create") echo "https://github.com/fixture/repository/pull/77" ;;
    "pr list") cat "$FIXTURE/open-pr" 2>/dev/null ;;
    "release view") cat "$FIXTURE/release-draft" 2>/dev/null ;;
esac
exit 0
"""

# Updates the workspace's Cargo.lock entries to the root manifest's version.
CARGO = r"""#!/bin/bash
echo "cargo $*" >> "$FIXTURE/calls.log"
status=$(cat "$FIXTURE/cargo-status" 2>/dev/null || echo 0)
((status == 0)) || exit "$status"
version=$(sed -n 's/^version = "\(.*\)"$/\1/p' Cargo.toml)
sed -i "s/^version = \".*/version = \"$version\"/" Cargo.lock
"""

# Checks nothing; pushes the tag as --push-tag does, unless told to refuse.
PUBLISH = r"""#!/bin/bash
echo "publish-release $*" >> "$FIXTURE/calls.log"
status=$(cat "$FIXTURE/publish-status" 2>/dev/null || echo 0)
((status == 0)) || exit "$status"
[[ " $* " == *" --push-tag "* ]] && git -C "$(dirname "$0")/.." push -q origin "refs/tags/v$2"
exit 0
"""

LAND = r"""#!/bin/bash
echo "land-pr $*" >> "$FIXTURE/calls.log"
exit "$(cat "$FIXTURE/land-status" 2>/dev/null || echo 0)"
"""

RECORD = r"""#!/bin/bash
echo "$(basename "$0") $*" >> "$FIXTURE/calls.log"
"""

SIGN = r"""#!/bin/bash
echo "ssh-keygen $*" >> "$FIXTURE/calls.log"
[[ ! -e $FIXTURE/sign-fails ]] || exit 255
touch "${@: -1}.sig"
"""


class ReleaseCut(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory(prefix="cagents-release-cut-", dir="/var/tmp")
        self.addCleanup(self.directory.cleanup)
        self.root = Path(self.directory.name)
        self.home = self.root / "home"
        self.repo = self.home / "repo"
        self.host = self.root / "host"
        self.bin = self.root / "bin"
        for directory in (self.repo / "scripts", self.bin, self.host / "repo/scripts",
                          self.host / ".local/state/cagents-prebuilt/bundles",
                          self.root / "waiting"):
            directory.mkdir(parents=True)
        for name, text in (("ssh", SSH), ("scp", SCP), ("gh", GH), ("ssh-keygen", SIGN), ("cargo", CARGO)):
            self.tool(self.bin / name, text)
        self.tool(self.host / "repo/scripts/prebuild-release", PREBUILD)
        self.tool(self.host / "repo/scripts/main-build-sentinel", SENTINEL)
        # The repository: the release scripts under test, and stand-ins for
        # what they hand on to.
        for name in ("release", "release-metadata"):
            shutil.copy2(ROOT / "scripts" / name, self.repo / "scripts" / name)
        self.tool(self.repo / "scripts/publish-release", PUBLISH)
        self.tool(self.repo / "scripts/land-pr", LAND)
        (self.repo / ".gitignore").write_text(".worktrees/\n")
        lock = ["version = 4\n"]
        for name, manifest in (("cagents", "Cargo.toml"), ("cagents-ui", "crates/cagents-ui/Cargo.toml"),
                               ("ceditor", "crates/ceditor/Cargo.toml"),
                               ("cagents-dictate", "crates/cagents-dictate/Cargo.toml")):
            path = self.repo / manifest
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(f'[package]\nname = "{name}"\nversion = "0.2.3"\n')
            lock.append(f'\n[[package]]\nname = "{name}"\nversion = "0.2.3"\n')
        (self.repo / "Cargo.lock").write_text("".join(lock))
        self.git("init", "-q", "-b", "main")
        self.git("config", "user.email", "fixture@example.invalid")
        self.git("config", "user.name", "fixture")
        self.base = self.commit("chore(release): set version 0.2.3")
        self.git("tag", "-a", "v0.2.3", "-m", "v0.2.3")
        self.set_version("0.2.4")
        self.bump = self.commit("chore(release): start 0.2.4 after v0.2.3")
        self.feature = self.commit("feat(chat): something people asked for")
        self.fix = self.commit("fix(terminal): something that broke")
        self.origin = self.root / "origin.git"
        subprocess.run(["git", "init", "-q", "--bare", str(self.origin)], check=True)
        self.git("remote", "add", "origin", str(self.origin))
        self.git("push", "-q", "origin", "main", "v0.2.3")
        self.bundles = []
        self.write_status()
        self.env = {key: value for key, value in os.environ.items() if not key.startswith(("CAGENTS_", "GIT_"))} | {
            "HOME": str(self.home), "FIXTURE": str(self.root), "PATH": f"{self.bin}:{os.environ['PATH']}",
            "CAGENTS_RELEASE_BUNDLES": str(self.root / "bundles"), "XDG_STATE_HOME": str(self.home / ".state")}

    def tool(self, path, text):
        path.write_text(text)
        path.chmod(0o755)

    def git(self, *args, cwd=None):
        return subprocess.run(["git", "-C", str(cwd or self.repo), *args], check=True, text=True,
                              capture_output=True).stdout.strip()

    def commit(self, message):
        self.git("add", ".")
        self.git("commit", "-q", "--allow-empty", "-m", message)
        return self.git("rev-parse", "HEAD")

    def set_version(self, version):
        for path in [*self.repo.rglob("Cargo.toml"), self.repo / "Cargo.lock"]:
            lines = path.read_text().splitlines(keepends=True)
            path.write_text("".join(f'version = "{version}"\n' if line.startswith('version = "') else line
                                    for line in lines))

    def bundle(self, sha, passed=True, where=None, version="0.2.4"):
        """A bundle of `sha` as the host's prebuild keeps it."""
        dist = (where or self.host / ".local/state/cagents-prebuilt/bundles") / sha / "dist"
        dist.mkdir(parents=True)
        (dist / f"cagents-{version}-{TARGET}.tar.gz").write_bytes(b"archive of " + sha.encode())
        (dist / "install.sh").write_text("#!/bin/sh\n")
        (dist / "SHA256SUMS").write_text("sums\n")
        notes = self.root / "prebuilt-notes.md"
        notes.write_text("Prebuilt bundle; the release writes its notes.\n")
        subprocess.run([str(ROOT / "scripts/release-metadata"), "write", "--dir", str(dist), "--version", version,
                        "--source-commit", sha, "--protocol", "9", "--notes", str(notes)],
                       check=True, capture_output=True)
        verdicts = self.host / ".local/state/cagents-main-sentinel/verdicts"
        verdicts.mkdir(parents=True, exist_ok=True)
        (verdicts / f"{sha}.json").write_text(json.dumps({"sha": sha, "passed": passed}))
        if where is None:
            self.bundles.append({"sha": sha, "version": version, "built_at": len(self.bundles)})
        self.write_status()
        return dist

    def write_status(self, building=None):
        (self.root / "status.json").write_text(json.dumps(
            {"bundles": self.bundles, "building": building, "requested": [], "target": None, "failures": [],
             "state": {}}))

    def release(self, *args, stdin=""):
        return subprocess.run([str(self.repo / "scripts/release"), *args], cwd=self.repo, env=self.env,
                              input=stdin, capture_output=True, text=True, timeout=HANG_GUARD)

    def log(self, name):
        path = self.root / name
        return path.read_text().splitlines() if path.exists() else []

    def test_cut_releases_the_newest_bundle_of_main_whose_tests_passed(self):
        self.bundle(self.feature)
        # Newer, but its tests failed; and one main does not contain.
        self.bundle(self.fix, passed=False)
        self.git("checkout", "-q", "-b", "side")
        side = self.commit("feat(side): never merged")
        self.git("checkout", "-q", "main")
        self.bundle(side)
        # Behind main, and with no terminal to ask: it names both choices.
        result = self.release("cut")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("1 commit(s) behind main", result.stdout)
        self.assertIn(f"--commit {self.feature}", result.stderr)
        self.assertFalse((self.root / "bundles/0.2.4").exists())
        result = self.release("cut", "--commit", self.feature, "--key", str(self.root / "key"))
        self.assertEqual(result.returncode, 0, result.stderr)
        bundle = self.root / "bundles/0.2.4"
        metadata = json.loads((bundle / "release-metadata.json").read_text())
        self.assertEqual((metadata["version"], metadata["source_commit"], metadata["protocol"]),
                         ("0.2.4", self.feature, 9))
        # Notes since the previous tag, not the prebuild's placeholder.
        self.assertIn("Something people asked for", metadata["notes"])
        self.assertNotIn("omething that broke", metadata["notes"])
        self.assertEqual(self.git("rev-parse", "v0.2.4^{commit}"), self.feature)
        self.assertTrue((bundle / "release-metadata.json.sig").exists())
        self.assertIn(f"ssh-keygen -Y sign -n cagents-release -f {self.root / 'key'} "
                      f"{bundle / 'release-metadata.json'}", self.log("calls.log"))
        self.assertEqual([line for line in self.log("prebuild.log") if line.split()[0] in ("pin", "unpin")],
                         [f"pin {self.feature}", f"unpin {self.feature}"] * 2)
        self.assertFalse((self.root / "bundles/0.2.4.partial").exists())

    def test_a_bundle_changed_after_it_was_built_is_refused(self):
        dist = self.bundle(self.fix)
        (dist / "install.sh").write_text("#!/bin/sh\nsomething else\n")
        result = self.release("cut")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("does not match what was built", result.stderr)
        self.assertFalse((self.root / "bundles/0.2.4").exists())
        self.assertIn(f"unpin {self.fix}", self.log("prebuild.log"))
        self.assertEqual(self.git("tag", "--list", "v0.2.4"), "")

    def test_wait_prebuilds_the_tip_and_requires_its_tests(self):
        self.bundle(self.feature)
        self.bundle(self.fix, where=self.root / "waiting")
        result = self.release("cut", "--wait")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn(f"request {self.fix}", self.log("prebuild.log"))
        self.assertIn(f"wait {self.fix}", self.log("prebuild.log"))
        self.assertTrue(any(f"--require-tests {self.fix}" in line for line in self.log("sentinel.log")))
        self.assertEqual(json.loads((self.root / "bundles/0.2.4/release-metadata.json").read_text())
                         ["source_commit"], self.fix)

    def test_wait_stops_when_the_tips_tests_fail(self):
        self.bundle(self.fix, where=self.root / "waiting")
        (self.root / "sentinel-status").write_text("1")
        result = self.release("cut", "--wait")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("fails its tests", result.stderr)
        self.assertFalse((self.root / "bundles/0.2.4").exists())

    def test_without_a_bundle_or_a_terminal_it_says_to_wait(self):
        result = self.release("cut")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("run with --wait", result.stderr)

    def test_a_version_already_tagged_on_origin_is_refused(self):
        self.bundle(self.feature)
        self.bundle(self.fix, where=self.root / "waiting")
        self.git("tag", "-a", "v0.2.4", "-m", "v0.2.4", self.feature)
        self.git("push", "-q", "origin", "v0.2.4")
        # The released version's bundle is no candidate...
        result = self.release("cut")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("no bundle of main", result.stderr)
        # ...and the tip, which declares it too, needs main's bump.
        result = self.release("cut", "--wait")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("version bump first", result.stderr)
        self.assertFalse((self.root / "bundles/0.2.4").exists())

    def test_bundles_of_a_released_version_are_passed_over_for_the_next(self):
        self.git("tag", "-a", "v0.2.4", "-m", "v0.2.4", self.feature)
        self.git("push", "-q", "origin", "v0.2.4")
        self.set_version("0.2.5")
        bump = self.commit("chore(release): start 0.2.5 after v0.2.4")
        self.git("push", "-q", "origin", "main")
        self.bundle(bump, version="0.2.5")
        # Newer than the bump's bundle, but of the released version.
        self.bundles.append({"sha": self.fix, "version": "0.2.4", "built_at": 99})
        self.bundle(self.fix, where=self.root / "elsewhere")
        result = self.release("cut")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.git("rev-parse", "v0.2.5^{commit}"), bump)

    def test_a_commit_off_main_is_refused(self):
        self.git("checkout", "-q", "-b", "side")
        side = self.commit("feat(side): never merged")
        self.git("checkout", "-q", "main")
        self.bundle(side)
        result = self.release("cut", "--commit", side)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("is not on main", result.stderr)

    def test_an_unpublished_cut_is_replaced_by_a_newer_one(self):
        self.bundle(self.feature)
        self.bundle(self.fix)
        self.assertEqual(self.release("cut", "--commit", self.feature).returncode, 0)
        self.assertEqual(self.git("rev-parse", "v0.2.4^{commit}"), self.feature)
        result = self.release("cut")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("Replacing the unpublished cut of v0.2.4", result.stdout)
        self.assertEqual(self.git("rev-parse", "v0.2.4^{commit}"), self.fix)
        self.assertEqual(json.loads((self.root / "bundles/0.2.4/release-metadata.json").read_text())
                         ["source_commit"], self.fix)

    def test_a_failed_signature_says_how_to_sign(self):
        self.bundle(self.fix)
        (self.root / "sign-fails").touch()
        result = self.release("cut", "--key", str(self.root / "key"))
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("cut but not signed", result.stderr)
        self.assertIn(f"ssh-keygen -Y sign -n cagents-release -f {self.root / 'key'}", result.stderr)
        self.assertIn(f"unpin {self.fix}", self.log("prebuild.log"))

    def test_an_evicted_bundle_is_reported(self):
        self.bundle(self.fix)
        (self.root / "pin-fails").touch()
        result = self.release("cut")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("may have been evicted", result.stderr)
        self.assertFalse((self.root / "bundles/0.2.4").exists())

    def cut_signed(self):
        self.bundle(self.fix)
        result = self.release("cut", "--key", str(self.root / "key"))
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_a_refused_publication_leaves_no_tag_on_origin(self):
        self.cut_signed()
        (self.root / "publish-status").write_text("1")
        result = self.release("publish", "0.2.4", stdin="0.2.4\n")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("run scripts/release publish 0.2.4 again", result.stderr)
        self.assertEqual(self.git("tag", "--list", "v0.2.4", cwd=self.origin), "")
        self.assertFalse(any(line.startswith("pr create") for line in self.log("gh.log")))

    def test_a_failed_bump_opens_nothing(self):
        self.cut_signed()
        (self.root / "cargo-status").write_text("101")
        result = self.release("publish", "0.2.4", stdin="0.2.4\n")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("version bump to 0.2.5 failed", result.stderr)
        self.assertFalse(any(line.startswith("pr create") for line in self.log("gh.log")))
        self.assertFalse(any(line.startswith("land-pr") for line in self.log("calls.log")))
        self.assertEqual(self.git("branch", "--list", "release/bump-0.2.5", cwd=self.origin), "")

    def test_publish_resumes_past_a_publication_and_a_pull_request_already_open(self):
        self.cut_signed()
        (self.root / "land-status").write_text("1")
        result = self.release("publish", "0.2.4", stdin="0.2.4\n")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("#77, which starts 0.2.5, did not land", result.stderr)
        # Run again: published already, and the bump's pull request is open.
        (self.root / "land-status").unlink()
        (self.root / "release-draft").write_text("false\n")
        (self.root / "open-pr").write_text("77\n")
        result = self.release("publish", "0.2.4")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("published already", result.stdout)
        self.assertEqual([line for line in self.log("calls.log") if line.startswith("publish-release")],
                         [f"publish-release --version 0.2.4 --dist {self.root / 'bundles/0.2.4'} --push-tag"])
        self.assertEqual(sum(line.startswith("pr create") for line in self.log("gh.log")), 1)
        self.assertEqual([line for line in self.log("calls.log") if line.startswith("land-pr")],
                         ["land-pr 77 -R CarsonBurke/cagents"] * 2)
        self.assertFalse((self.repo / ".worktrees/release-bump-0.2.5").exists())

    def test_a_bump_attempt_that_opened_nothing_is_rebuilt(self):
        self.cut_signed()
        self.git("push", "-q", "origin", "main")
        (self.root / "release-draft").write_text("false\n")
        # A leftover worktree and pushed branch from an attempt that opened no pull request.
        self.git("worktree", "add", "-q", "-b", "release/bump-0.2.5", str(self.repo / ".worktrees/release-bump-0.2.5"))
        self.git("push", "-q", "origin", "release/bump-0.2.5")
        result = self.release("publish", "0.2.4")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn('version = "0.2.5"',
                      self.git("show", "release/bump-0.2.5:Cargo.lock", cwd=self.origin))

    def test_publish_pushes_the_tag_then_lands_the_next_versions_bump(self):
        self.bundle(self.fix)
        self.assertEqual(self.release("cut", "--key", str(self.root / "key")).returncode, 0)
        self.git("push", "-q", "origin", "main")
        result = self.release("publish", "0.2.4", stdin="0.2.4\n")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.git("rev-parse", "v0.2.4^{commit}", cwd=self.origin), self.fix)
        self.assertIn(f"publish-release --version 0.2.4 --dist {self.root / 'bundles/0.2.4'} --push-tag",
                      self.log("calls.log"))
        create = next(line for line in self.log("gh.log") if line.startswith("pr create"))
        self.assertIn("--head release/bump-0.2.5", create)
        self.assertIn("--title chore(release): start 0.2.5 after v0.2.4", create)
        self.assertIn("land-pr 77 -R CarsonBurke/cagents", self.log("calls.log"))
        branch = self.git("show", "release/bump-0.2.5:crates/ceditor/Cargo.toml", cwd=self.origin)
        self.assertIn('version = "0.2.5"', branch)
        lock = self.git("show", "release/bump-0.2.5:Cargo.lock", cwd=self.origin)
        self.assertEqual(lock.count('version = "0.2.5"'), 4)
        self.assertEqual(self.git("log", "-1", "--format=%s", "release/bump-0.2.5", cwd=self.origin),
                         "chore(release): start 0.2.5 after v0.2.4")
        self.assertFalse((self.repo / ".worktrees/release-bump-0.2.5").exists())

    def test_publish_needs_no_bump_once_main_has_moved_on(self):
        self.bundle(self.fix)
        self.assertEqual(self.release("cut", "--key", str(self.root / "key")).returncode, 0)
        self.set_version("0.2.4")
        for path in self.repo.rglob("Cargo.toml"):
            path.write_text(path.read_text().replace('version = "0.2.4"', 'version = "0.3.0"'))
        self.commit("chore(release): start 0.3.0")
        self.git("push", "-q", "origin", "main")
        result = self.release("publish", "0.2.4", stdin="0.2.4\n")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("needs no version bump", result.stdout)
        self.assertFalse(any(line.startswith("pr ") for line in self.log("gh.log")))


if __name__ == "__main__":
    unittest.main()
