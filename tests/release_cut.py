#!/usr/bin/env python3
"""Exercise `scripts/release cut` and `publish` against a private repository,
a stand-in build host reached through stand-in ssh and scp, and stand-in
GitHub, signing and publishing tools. Nothing builds, signs or publishes."""

import fcntl
import hashlib
import json
import os
from pathlib import Path
import shutil
import signal
import subprocess
import time
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
# The remote side outlives the client, which an interrupt ends at once.
HOME=$FIXTURE/host bash -c "$*" <&0 &
trap 'exit 255' INT TERM
wait "$!"
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
        if [[ -p $fixture/wait-ready ]]; then
            trap 'touch "$fixture/wait-cancelled"; exit 143' TERM
            printf 'waiting\n' > "$fixture/wait-ready"
            read -r -t 15 unused <>"$fixture/wait-release" || exit 99
        fi
        mv "$fixture/waiting/$2" "$HOME/.local/state/cagents-prebuilt/bundles/$2"
        echo "$2" ;;
esac
"""

SENTINEL = r"""#!/bin/bash
echo "$*" >> "$FIXTURE/sentinel.log"
if [[ -p $FIXTURE/wait-ready ]]; then
    read -r ready < "$FIXTURE/wait-ready"
fi
exit "$(cat "$FIXTURE/sentinel-status" 2>/dev/null || echo 0)"
"""

GH = r"""#!/bin/bash
echo "$*" >> "$FIXTURE/gh.log"
case "$1 $2" in
    "pr create") echo "https://github.com/fixture/repository/pull/77" ;;
    "pr list") cat "$FIXTURE/open-pr" 2>/dev/null ;;
    "release view")
        if [[ " $* " == *" tagName "* ]]; then cat "$FIXTURE/published" 2>/dev/null || echo v0.2.3
        elif [[ -e $FIXTURE/view-fails ]]; then echo "HTTP 502: Bad Gateway" >&2; exit 1
        else cat "$FIXTURE/release-draft" 2>/dev/null; fi ;;
    "run list") cat "$FIXTURE/runs" 2>/dev/null \
        || echo '[{"status": "completed", "conclusion": "success", "url": "https://github.com/CarsonBurke/cagents/actions/runs/1"}]' ;;
    "release download")
        version=${3#v} patterns=()
        while (($#)); do [[ $1 == -D ]] && directory=$2; [[ $1 == -p ]] && patterns+=("$2"); shift; done
        cd "$directory"
        # A draft serves the files the fixture left for it that match.
        if [[ -d $FIXTURE/drafts/v$version ]]; then
            for file in "$FIXTURE/drafts/v$version"/*; do
                for pattern in "${patterns[@]}"; do
                    [[ ${file##*/} != $pattern ]] || cp "$file" .
                done
            done
            exit 0
        fi
        echo "archive of $version" > "cagents-$version-x86_64-unknown-linux-gnu.tar.gz"
        echo "#!/bin/sh" > install.sh
        sha256sum "cagents-$version-x86_64-unknown-linux-gnu.tar.gz" install.sh > SHA256SUMS ;;
    "variable list") [[ ! -e $FIXTURE/variables-fail ]] || exit 1
        echo '[{"name":"RELEASE_SIGNING_PUBLIC_KEY","value":"ssh-ed25519 FIXTURE"}]' ;;
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

# The commit's rehearsal harness: records how it is run, and reports the
# fixture's outcome and update status.
REHEARSE = r"""#!/bin/bash
root=$2 command=$3
echo "$0 $command ${*:4}" >> "$FIXTURE/rehearse.log"
echo "$root" >> "$FIXTURE/rehearse-roots.log"
case $command in
    cleanup) rm -rf "$root" ;;
    run)
        feed=; while (($#)); do
            [[ $1 == --feed ]] && feed=$2
            [[ $1 != --new-dir ]] || cat "$2"/cagents-*.tar.gz > "$FIXTURE/rehearsed-archive"
            shift
        done
        [[ -z $feed ]] || ls "$feed/download" | sed "s|^|feed $feed offers |" >> "$FIXTURE/rehearse-feed.log"
        [[ -z $feed ]] || ls "$feed/latest/download" | sed "s|^|feed $feed latest |" >> "$FIXTURE/rehearse-feed.log"
        mkdir -p "$root/home/.local/share/cagents-install/.update"
        outcome=$(cat "$FIXTURE/rehearse-outcome" 2>/dev/null || echo PASS)
        printf 'Total duration: 61.2s\nOutcome: %s\n' "$outcome" > "$root/report.txt"
        [[ $outcome == PASS ]] || printf 'Regressions / evidence gaps:\nthe shell terminal is gone\n\nSteps\n' >> "$root/report.txt"
        cat "$FIXTURE/rehearse-status" 2>/dev/null > "$root/home/.local/share/cagents-install/.update/status.json" \
            || echo '{"phase": "Complete"}' > "$root/home/.local/share/cagents-install/.update/status.json"
        [[ ! -e $FIXTURE/rehearse-plants ]] || mkdir -p "$(cat "$FIXTURE/rehearse-plants")"
        if [[ -e $FIXTURE/rehearse-hangs ]]; then
            trap 'echo stopped >> "$FIXTURE/rehearse-stopped"; exit 143' TERM
            echo running > "$FIXTURE/rehearse-running"
            sleep 60 & wait
        fi
        [[ $outcome == PASS ]] ;;
esac
"""

# The workflow's admission, as asked before pushing a tag: refuses when the
# fixture says so, and otherwise says whether Actions would sign.
ADMIT = r"""#!/bin/bash
echo "release-actions $* public=$RELEASE_SIGNING_PUBLIC_KEY" >> "$FIXTURE/calls.log"
[[ ! -e $FIXTURE/admit-refuses ]] || { echo "release-actions: refused by the fixture" >&2; exit 1; }
echo "sign=$(cat "$FIXTURE/admit-sign" 2>/dev/null || echo true)" >> "$GITHUB_OUTPUT"
"""

# Exits with each line of land-statuses in turn, then 0.
LAND = r"""#!/bin/bash
echo "land-pr $*" >> "$FIXTURE/calls.log"
statuses=$FIXTURE/land-statuses
[[ -s $statuses ]] || exit 0
status=$(head -n1 "$statuses")
sed -i 1d "$statuses"
exit "$status"
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
        self.directory = tempfile.TemporaryDirectory(prefix="cagents-release-cut-",
                                                     dir=os.environ.get("TMPDIR", "/var/tmp"))
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
        self.tool(self.repo / "scripts/rehearse-update", REHEARSE)
        self.tool(self.repo / "scripts/release-actions", ADMIT)
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
        # The host's checkout of the same repository.
        host = self.host / "repo"
        self.git("init", "-q", cwd=host)
        self.git("remote", "add", "origin", str(self.origin), cwd=host)
        self.git("fetch", "-q", "origin", cwd=host)
        self.bundles = []
        self.write_status()
        self.env = {key: value for key, value in os.environ.items() if not key.startswith(("CAGENTS_", "GIT_"))} | {
            "HOME": str(self.home), "FIXTURE": str(self.root), "PATH": f"{self.bin}:{os.environ['PATH']}",
            "CAGENTS_RELEASE_BUNDLES": str(self.root / "bundles"), "XDG_STATE_HOME": str(self.home / ".state"),
            "CAGENTS_RELEASE_REHEARSAL_ROOT": str(self.root / "cr")}

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
        subprocess.run(f"sha256sum cagents-{version}-{TARGET}.tar.gz install.sh > SHA256SUMS", shell=True,
                       cwd=dist, check=True)
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

    def assert_rehearsal_report_kept(self, result, root):
        kept = root
        if root.is_relative_to("/dev/shm"):
            self.assertFalse(root.exists())
            kept = self.host / ".cache/cagents-release-rehearsal/failed-0.2.4"
        self.assertIn(f"in {kept}", result.stderr)
        self.assertTrue((kept / "report.txt").is_file())

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

    def test_wait_stops_when_testing_cannot_produce_a_verdict(self):
        (self.root / "sentinel-status").write_text("3")
        os.mkfifo(self.root / "wait-ready")
        os.mkfifo(self.root / "wait-release")
        result = self.release("cut", "--wait")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("no verdict (status 3)", result.stderr)
        self.assertTrue((self.root / "wait-cancelled").exists())
        self.assertFalse((self.root / "bundles/0.2.4").exists())
        self.assertEqual(self.git("tag", "--list", "v0.2.4"), "")

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
        (self.root / "release-draft").write_text("false\n")
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

    def test_rehearse_updates_the_published_release_to_the_cut_with_the_cut_commits_harness(self):
        self.cut_signed()
        result = self.release("rehearse", "0.2.4")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("Rehearsing the in-app update from 0.2.3 to 0.2.4", result.stdout)
        self.assertIn("Outcome: PASS", result.stdout)
        self.assertIn("Update status: Complete", result.stdout)
        self.assertRegex(result.stdout, r"Rehearsed 0\.2\.3 to 0\.2\.4 in \d+ min \d+ s")
        harness = self.host / "repo/.worktrees/.rehearse-release"
        run = next(line for line in self.log("rehearse.log") if " run " in line)
        old = self.host / ".cache/cagents-release-rehearsal/0.2.3"
        new = self.host / ".cache/cagents-release-rehearsal/cut-0.2.4"
        feed = self.host / ".cache/cagents-release-rehearsal/feed-0.2.4"
        self.assertEqual(run, f"{harness}/scripts/rehearse-update run --old-dir {old} --old-version 0.2.3 "
                              f"--old-installer {old}/install.sh --new-dir {new} --new-version 0.2.4 "
                              f"--new-installer {new}/install.sh --answer yes --ended-provider "
                              f"--feed {feed} --empty-window --restart-after-update")
        # The feed the published build read offered the signed cut; it goes once the rehearsal passes.
        self.assertIn(f"feed {feed} offers v0.2.4", self.log("rehearse-feed.log"))
        self.assertFalse(feed.exists())
        # The cut's own bytes, not the host's store; its copy and the
        # incoming copy go once it passes, and the report stays as evidence.
        self.assertEqual((self.root / "rehearsed-archive").read_bytes(),
                         (self.root / f"bundles/0.2.4/cagents-0.2.4-{TARGET}.tar.gz").read_bytes())
        self.assertFalse(new.exists())
        cache = self.host / ".cache/cagents-release-rehearsal"
        self.assertEqual(sorted(path.name for path in cache.iterdir()), ["0.2.3", "lock", "passed-0.2.4.txt"])
        self.assertIn("Outcome: PASS", (cache / "passed-0.2.4.txt").read_text())
        self.assertFalse((self.root / "cr").exists())
        self.assertFalse(harness.exists())
        # The published release is fetched once and kept.
        self.assertEqual(sum(line.startswith("release download") for line in self.log("gh.log")), 1)
        self.assertEqual(self.release("rehearse", "0.2.4").returncode, 0)
        self.assertEqual(sum(line.startswith("release download") for line in self.log("gh.log")), 1)

    def test_the_default_root_and_a_harness_at_another_commit(self):
        self.cut_signed()
        self.env.pop("CAGENTS_RELEASE_REHEARSAL_ROOT")
        # Never the real /var/tmp/cr, which a rehearsal on this host may hold.
        parent = self.root / "parents/cr"
        self.env["CAGENTS_RELEASE_REHEARSAL_PARENTS"] = str(parent)
        # A stray directory where the harness goes, which Git must not walk out of.
        stray = self.host / "repo/.worktrees/.rehearse-release"
        stray.mkdir(parents=True)
        (stray / "leftover").touch()
        (self.root / "rehearse-outcome").write_text("FAIL\n")
        result = self.release("rehearse", "0.2.4")
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(self.log("rehearse-roots.log")[0], str(parent / "release-0.2.4"))
        self.assert_rehearsal_report_kept(result, parent / "release-0.2.4")
        self.assertEqual(self.git("rev-parse", "HEAD", cwd=stray), self.fix)
        self.assertFalse((stray / "leftover").exists())

    def test_an_earlier_failed_rehearsal_is_replaced(self):
        self.cut_signed()
        earlier = self.root / "cr"
        earlier.mkdir()
        (earlier / ".rehearse-update").write_text(f"{earlier}\n")
        (earlier / "report.txt").write_text("Outcome: FAIL\n")
        result = self.release("rehearse", "0.2.4")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("Replacing the earlier rehearsal", result.stdout)
        commands = [line.split()[1] for line in self.log("rehearse.log")]
        self.assertEqual(commands, ["cleanup", "run", "cleanup"])

    def test_a_rehearsal_whose_update_failed_or_was_refused_does_not_pass_either_way(self):
        # Reopened windows would show "Update failed" or "Update not
        # installed", whoever's worker wrote it: in the app, the published
        # build's (#3877).
        self.cut_signed()
        for mode in ((), ("--staged",)):
            for status, shown in (('{"phase": "Failed"}', "Failed"),
                                  ('{"phase": "Failed", "refusal": {"kind": "busy"}}', "Failed refused"),
                                  ("", "absent")):
                (self.root / "rehearse-status").write_text(status)
                result = self.release("rehearse", "0.2.4", *mode)
                self.assertNotEqual(result.returncode, 0, mode)
                self.assertIn(f"Update status: {shown}\n", result.stdout)
                self.assertIn(f"the update ended {shown}, not Complete", result.stderr)
                self.assert_rehearsal_report_kept(result, self.root / "cr")

    def test_the_root_avoids_a_parent_under_a_stray_git_and_a_failure_in_memory_keeps_only_its_evidence(self):
        self.cut_signed()
        self.env.pop("CAGENTS_RELEASE_REHEARSAL_ROOT")
        marked, clean = self.root / "marked", self.root / "clean"
        (marked / ".git").mkdir(parents=True)
        self.env["CAGENTS_RELEASE_REHEARSAL_PARENTS"] = f"{marked}/cr:{clean}/cr"
        result = self.release("rehearse", "0.2.4")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.log("rehearse-roots.log")[0], f"{clean}/cr/release-0.2.4")
        self.assertFalse((marked / "cr").exists())
        self.assertFalse((clean / "cr/release-0.2.4").exists())
        # A failure in memory keeps its report and screenshots on disk, and
        # frees the rest.
        memory = Path(tempfile.mkdtemp(prefix="cr-test-", dir="/dev/shm"))
        self.addCleanup(shutil.rmtree, memory, True)
        self.env["CAGENTS_RELEASE_REHEARSAL_PARENTS"] = f"{marked}/cr:{memory}/cr"
        (self.root / "rehearse-outcome").write_text("FAIL\n")
        result = self.release("rehearse", "0.2.4")
        self.assertNotEqual(result.returncode, 0)
        kept = self.host / ".cache/cagents-release-rehearsal/failed-0.2.4"
        self.assertIn(f"report and screenshots are in {kept}", result.stderr)
        self.assertIn("Outcome: FAIL", (kept / "report.txt").read_text())
        self.assertFalse((memory / "cr/release-0.2.4").exists())
        (self.root / "rehearse-outcome").unlink()
        # A .git that appears above the root while it runs taints a failure.
        tainted = self.root / "tainted"
        self.env["CAGENTS_RELEASE_REHEARSAL_PARENTS"] = f"{tainted}/cr"
        (self.root / "rehearse-outcome").write_text("FAIL\n")
        (self.root / "rehearse-plants").write_text(str(tainted / ".git"))
        result = self.release("rehearse", "0.2.4")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn(f"{tainted}/.git appeared above {tainted}/cr/release-0.2.4 during the rehearsal", result.stderr)
        (self.root / "rehearse-outcome").unlink()
        (self.root / "rehearse-plants").unlink()
        # Every parent marked: it says so rather than guess.
        self.env["CAGENTS_RELEASE_REHEARSAL_PARENTS"] = f"{marked}/cr"
        result = self.release("rehearse", "0.2.4")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("has a .git above it; set CAGENTS_RELEASE_REHEARSAL_ROOT", result.stderr)

    def test_an_interrupt_stops_the_hosts_run_and_frees_its_lock(self):
        self.cut_signed()
        (self.root / "rehearse-hangs").touch()
        release = subprocess.Popen([str(self.repo / "scripts/release"), "rehearse", "0.2.4"], cwd=self.repo,
                                   env=self.env, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE,
                                   stderr=subprocess.PIPE, text=True, start_new_session=True)
        self.addCleanup(release.kill)
        deadline = time.monotonic() + HANG_GUARD
        while not (self.root / "rehearse-running").exists():
            self.assertIsNone(release.poll(), release.stderr.read() if release.poll() is not None else "")
            self.assertLess(time.monotonic(), deadline)
            time.sleep(0.1)
        # Ctrl-C reaches the terminal's foreground group: here, and the ssh client.
        os.killpg(release.pid, signal.SIGINT)
        _, stderr = release.communicate(timeout=HANG_GUARD)
        self.assertEqual(release.returncode, 130, stderr)
        self.assertIn("Stopping the rehearsal", stderr)
        deadline = time.monotonic() + 30
        while not (self.root / "rehearse-stopped").exists():
            self.assertLess(time.monotonic(), deadline, "the host's harness was never stopped")
            time.sleep(0.1)
        cache = self.host / ".cache/cagents-release-rehearsal"
        while any(path.name.startswith("incoming-") for path in cache.iterdir()):
            self.assertLess(time.monotonic(), deadline, "the host kept the interrupted run's files")
            time.sleep(0.1)
        (self.root / "rehearse-hangs").unlink()
        self.assertEqual(self.release("rehearse", "0.2.4").returncode, 0)

    def test_a_root_the_hosts_shell_would_reparse_is_refused(self):
        self.cut_signed()
        self.env["CAGENTS_RELEASE_REHEARSAL_ROOT"] = str(self.root / "my rehearsal")
        result = self.release("rehearse", "0.2.4")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("may hold only letters", result.stderr)
        self.assertFalse((self.root / "rehearse.log").exists())

    def test_a_staged_rehearsal_needs_no_signature_and_runs_the_cuts_upgrade(self):
        self.bundle(self.fix)
        self.assertEqual(self.release("cut").returncode, 0)
        refused = self.release("rehearse", "0.2.4")
        self.assertNotEqual(refused.returncode, 0)
        self.assertIn("sign", refused.stderr)
        result = self.release("rehearse", "0.2.4", "--staged")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("Rehearsing the staged update", result.stdout)
        run = next(line for line in self.log("rehearse.log") if " run " in line)
        self.assertTrue(run.endswith("--answer yes --ended-provider"), run)
        self.assertIn("--staged and --from-draft are for rehearse", self.release("publish", "0.2.4", "--staged").stderr)

    def test_a_failed_rehearsal_reports_what_regressed(self):
        self.cut_signed()
        (self.root / "rehearse-outcome").write_text("FAIL\n")
        result = self.release("rehearse", "0.2.4")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("the shell terminal is gone", result.stdout)
        self.assertIn("did not pass", result.stderr)

    def test_a_damaged_copy_or_download_is_refused(self):
        self.cut_signed()
        (self.root / f"bundles/0.2.4/cagents-0.2.4-{TARGET}.tar.gz").write_bytes(b"something else")
        result = self.release("rehearse", "0.2.4")
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(self.log("rehearse.log"), [])

    def test_a_damaged_cached_release_is_fetched_again(self):
        self.cut_signed()
        self.assertEqual(self.release("rehearse", "0.2.4").returncode, 0)
        old = self.host / ".cache/cagents-release-rehearsal/0.2.3"
        (old / "install.sh").write_text("changed\n")
        result = self.release("rehearse", "0.2.4")
        self.assertIn("was damaged", result.stderr)
        self.assertFalse(old.exists())
        self.assertEqual(self.release("rehearse", "0.2.4").returncode, 0)

    def test_another_rehearsal_running_is_refused(self):
        self.cut_signed()
        lock = self.host / ".cache/cagents-release-rehearsal/lock"
        lock.parent.mkdir(parents=True, exist_ok=True)
        with lock.open("a") as held:
            fcntl.flock(held, fcntl.LOCK_EX)
            result = self.release("rehearse", "0.2.4")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("another rehearsal is running", result.stderr)

    def test_rehearse_needs_a_cut_and_an_older_release_published(self):
        self.assertIn("cut 0.2.4 first", self.release("rehearse", "0.2.4").stderr)
        self.cut_signed()
        for published in ("v0.2.4", "v0.3.0"):
            (self.root / "published").write_text(published + "\n")
            self.assertIn("nothing to rehearse", self.release("rehearse", "0.2.4").stderr)

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
        (self.root / "land-statuses").write_text("1\n")
        result = self.release("publish", "0.2.4", stdin="0.2.4\n")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("#77, which starts 0.2.5, did not land", result.stderr)
        # Run again: published already, and the bump's pull request is open.
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

    def test_a_bump_that_conflicts_with_main_is_made_again(self):
        self.cut_signed()
        self.git("push", "-q", "origin", "main")
        (self.root / "release-draft").write_text("false\n")
        (self.root / "open-pr").write_text("77\n")
        (self.root / "land-statuses").write_text("3\n")
        # The open pull request's branch, from before main moved on.
        self.git("push", "-q", "origin", f"{self.feature}:refs/heads/release/bump-0.2.5")
        later = self.commit("fix(deps): a change beside the version lines")
        self.git("push", "-q", "origin", "main")
        result = self.release("publish", "0.2.4")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("conflicts with main; making the bump again", result.stdout)
        self.assertEqual([line for line in self.log("calls.log") if line.startswith("land-pr")],
                         ["land-pr 77 -R CarsonBurke/cagents"] * 2)
        self.assertEqual(self.git("rev-parse", "release/bump-0.2.5^", cwd=self.origin), later)
        self.assertFalse(any(line.startswith("pr create") for line in self.log("gh.log")))

    def test_a_tag_on_origin_that_was_never_published_says_to_publish(self):
        # Publishing pushed the signed cut's tag, then failed.
        self.bundle(self.fix)
        self.assertEqual(self.release("cut", "--commit", self.fix, "--key", str(self.root / "key")).returncode, 0)
        self.git("push", "-q", "origin", "main", "v0.2.4")
        result = self.release("cut", "--commit", self.fix)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("finish it with scripts/release publish 0.2.4", result.stderr)

    def test_commit_and_wait_together_are_refused(self):
        result = self.release("cut", "--wait", "--commit", self.fix)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("--wait releases main's tip", result.stderr)

    def test_a_failed_recut_keeps_the_earlier_cut(self):
        self.bundle(self.feature)
        dist = self.bundle(self.fix)
        self.assertEqual(self.release("cut", "--commit", self.feature).returncode, 0)
        (dist / "install.sh").write_text("#!/bin/sh\nchanged\n")
        result = self.release("cut")
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(self.git("rev-parse", "v0.2.4^{commit}"), self.feature)
        self.assertEqual(json.loads((self.root / "bundles/0.2.4/release-metadata.json").read_text())
                         ["source_commit"], self.feature)

    def test_a_signed_cut_is_not_replaced_without_asking(self):
        self.bundle(self.feature)
        self.bundle(self.fix)
        self.assertEqual(self.release("cut", "--commit", self.feature, "--key", str(self.root / "key")).returncode, 0)
        result = self.release("cut")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("signed already", result.stdout)
        self.assertEqual(self.git("rev-parse", "v0.2.4^{commit}"), self.feature)

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


    def test_cut_for_actions_tags_the_chosen_commit_and_copies_and_signs_nothing(self):
        self.bundle(self.fix)
        result = self.release("cut", "--actions")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.git("rev-parse", "v0.2.4^{commit}"), self.fix)
        self.assertIn("Something people asked for", result.stdout)
        self.assertIn("scripts/release publish 0.2.4 --actions\n  scripts/release rehearse 0.2.4 --from-draft\n"
                      "  scripts/release publish 0.2.4 --actions", result.stdout)
        self.assertFalse((self.root / "bundles/0.2.4").exists())
        self.assertFalse(any(line.startswith("ssh-keygen") for line in self.log("calls.log")))
        # Actions signs; a local key has no place in it.
        refused = self.release("cut", "--actions", "--key", str(self.root / "key"))
        self.assertIn("drop --key", refused.stderr)

    def test_publish_in_actions_pushes_the_tag_and_starts_the_next_version_once_published(self):
        self.bundle(self.fix)
        self.assertEqual(self.release("cut", "--actions").returncode, 0)
        self.git("push", "-q", "origin", "main")
        self.assertIn("not pushed", self.release("publish", "0.2.4", "--actions", stdin="0.2.3\n").stderr)
        self.assertEqual(self.git("tag", "--list", "v0.2.4", cwd=self.origin), "")
        result = self.release("publish", "0.2.4", "--actions", "--yes")
        # Pushed, not published: it says what to do next, and is not done.
        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertIn("release-actions admit v0.2.4 public=ssh-ed25519 FIXTURE", self.log("calls.log"))
        self.assertEqual(self.git("rev-parse", "v0.2.4^{commit}", cwd=self.origin), self.fix)
        self.assertIn("scripts/release rehearse 0.2.4 --from-draft\n  scripts/release publish 0.2.4 --actions",
                      result.stdout)
        # Nothing is published here, and main keeps its version until Actions publishes.
        self.assertFalse(any(line.startswith(("publish-release", "land-pr")) for line in self.log("calls.log")))
        result = self.release("publish", "0.2.4", "--actions", "--yes")
        self.assertEqual(result.returncode, 1)
        self.assertIn("is pushed and has no draft yet", result.stderr)
        self.assertFalse(any(line.startswith("land-pr") for line in self.log("calls.log")))
        (self.root / "release-draft").write_text("false\n")
        result = self.release("publish", "0.2.4", "--actions", "--yes")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("published already", result.stdout)
        self.assertIn("land-pr 77 -R CarsonBurke/cagents", self.log("calls.log"))

    def drafted(self):
        """Tags a commit of trust generation 2 for Actions, pushes the tag as
        publish --actions does, and leaves the release the workflow would
        have signed as its draft, every file under its published name."""
        feed = self.repo / "src/update/feed.rs"
        feed.parent.mkdir(parents=True, exist_ok=True)
        feed.write_text("const GENERATION: usize = 2;\n")
        sha = self.commit("feat(update): trust generations")
        self.git("push", "-q", "origin", "main")
        dist = self.bundle(sha)
        result = self.release("cut", "--actions", "--commit", sha)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.git("push", "-q", "origin", "v0.2.4")
        draft = self.root / "drafts/v0.2.4"
        draft.mkdir(parents=True)
        for path in dist.iterdir():
            name = "release-metadata-2.json" if path.name == "release-metadata.json" else path.name
            shutil.copyfile(path, draft / name)
        (draft / "release-metadata-2.json.sig").write_text("signature\n")
        # What generation 1 builds read: the release that began generation 2.
        (draft / "release-metadata.json").write_text('{"version": "0.2.4"}\n')
        (draft / "release-metadata.json.sig").write_text("bridge signature\n")
        (self.root / "release-draft").write_text("true\n")
        return draft

    def test_a_draft_is_rehearsed_as_github_serves_it_and_its_pass_names_its_metadata(self):
        draft = self.drafted()
        result = self.release("rehearse", "0.2.4", "--from-draft")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("Rehearsing the in-app update from 0.2.3 to 0.2.4 as drafted on GitHub", result.stdout)
        # The host fetched what the updater reads of the draft; nothing went from here.
        self.assertIn("release download v0.2.4 -R CarsonBurke/cagents", "\n".join(self.log("gh.log")))
        self.assertEqual((self.root / "rehearsed-archive").read_bytes(),
                         (draft / f"cagents-0.2.4-{TARGET}.tar.gz").read_bytes())
        feed = self.host / ".cache/cagents-release-rehearsal/feed-0.2.4"
        served = self.log("rehearse-feed.log")
        self.assertIn(f"feed {feed} offers v0.2.4", served)
        self.assertEqual([line for line in served if " latest " in line],
                         [f"feed {feed} latest {name}" for name in ("release-metadata-2.json", "release-metadata-2.json.sig",
                                                                     "release-metadata.json", "release-metadata.json.sig")])
        # The pass names the metadata it updated to, as the draft serves it.
        digest = hashlib.sha256((draft / "release-metadata-2.json").read_bytes()).hexdigest()
        passed = (self.host / ".cache/cagents-release-rehearsal/passed-0.2.4.txt").read_text().splitlines()
        self.assertEqual(passed[:2], [f"Rehearsed metadata: sha256 {digest}", "Rehearsed from: 0.2.3"])
        self.assertIn("Outcome: PASS", passed)
        self.assertIn(f"run list -R CarsonBurke/cagents --workflow release.yml --branch v0.2.4 --event push --limit 1 "
                      f"--json status,conclusion,url", self.log("gh.log"))

    def test_a_draft_is_rehearsed_only_once_the_workflow_has_finished_it(self):
        self.drafted()
        runs = self.root / "runs"
        url = "https://github.com/CarsonBurke/cagents/actions/runs/7"
        for run, said in ((f'[{{"status": "in_progress", "conclusion": "", "url": "{url}"}}]',
                           f"the release workflow run of v0.2.4 is still in_progress: its draft is not finished yet ({url})"),
                          (f'[{{"status": "completed", "conclusion": "failure", "url": "{url}"}}]',
                           f"the release workflow run of v0.2.4 ended failure, so its draft may be incomplete or "
                           f"unverified; rerun its failed jobs ({url})"),
                          ("[]", "no release workflow run of v0.2.4 is found")):
            runs.write_text(run)
            for command in (("rehearse", "0.2.4", "--from-draft"), ("publish", "0.2.4", "--actions", "--yes")):
                result = self.release(*command)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn(said, result.stderr)
        self.assertEqual(self.log("rehearse.log"), [])
        self.assertFalse(any(line.startswith("publish-release") for line in self.log("calls.log")))

    def test_draft_metadata_of_another_version_or_commit_is_not_rehearsed(self):
        draft = self.drafted()
        metadata = draft / "release-metadata-2.json"
        original = json.loads(metadata.read_text())
        for changed in ({"version": "0.2.5"}, {"source_commit": self.fix}):
            metadata.write_text(json.dumps(original | changed))
            result = self.release("rehearse", "0.2.4", "--from-draft")
            self.assertNotEqual(result.returncode, 0)
            self.assertIn(f"release-metadata-2.json is not 0.2.4's from {self.git('rev-parse', 'v0.2.4^{commit}')}",
                          result.stderr)
        self.assertEqual(self.log("rehearse.log"), [])

    def test_the_state_of_the_release_is_read_not_guessed(self):
        self.drafted()
        (self.root / "view-fails").touch()
        for command in (("rehearse", "0.2.4", "--from-draft"), ("publish", "0.2.4", "--actions", "--yes")):
            self.assertIn("cannot read the release v0.2.4 on GitHub: HTTP 502: Bad Gateway", self.release(*command).stderr)
        (self.root / "view-fails").unlink()
        (self.root / "release-draft").write_text("false\n")
        self.assertIn("v0.2.4 is published already; there is no draft to rehearse",
                      self.release("rehearse", "0.2.4", "--from-draft").stderr)

    def test_publish_in_actions_publishes_the_draft_only_once_its_rehearsal_passed(self):
        draft = self.drafted()
        refused = self.release("publish", "0.2.4", "--actions", "--yes")
        self.assertNotEqual(refused.returncode, 0)
        self.assertIn("no rehearsal of v0.2.4's draft has passed on fw16, so it is not published; "
                      "rehearse it: scripts/release rehearse 0.2.4 --from-draft", refused.stderr)
        self.assertEqual(self.release("rehearse", "0.2.4", "--from-draft").returncode, 0)
        self.assertIn("not published", self.release("publish", "0.2.4", "--actions", stdin="0.2.3\n").stderr)
        self.assertFalse(any(line.startswith(("publish-release", "land-pr")) for line in self.log("calls.log")))
        # A release published since makes it an update from something else.
        (self.root / "published").write_text("v0.2.2\n")
        self.assertIn("was rehearsed updating from 0.2.3, but the newest published release is now 0.2.2; "
                      "rehearse it again", self.release("publish", "0.2.4", "--actions", "--yes").stderr)
        (self.root / "published").unlink()
        result = self.release("publish", "0.2.4", "--actions", "--yes")
        self.assertEqual(result.returncode, 0, result.stderr)
        digest = hashlib.sha256((draft / "release-metadata-2.json").read_bytes()).hexdigest()
        self.assertIn(f"publish-release --version 0.2.4 --promote {digest}", self.log("calls.log"))
        self.assertIn("land-pr 77 -R CarsonBurke/cagents", self.log("calls.log"))

    def test_a_failed_rehearsal_withdraws_an_earlier_pass(self):
        self.drafted()
        self.assertEqual(self.release("rehearse", "0.2.4", "--from-draft").returncode, 0)
        (self.root / "rehearse-outcome").write_text("FAIL\n")
        self.assertNotEqual(self.release("rehearse", "0.2.4", "--from-draft").returncode, 0)
        self.assertIn("no rehearsal", self.release("publish", "0.2.4", "--actions", "--yes").stderr)
        self.assertFalse(any(line.startswith("publish-release") for line in self.log("calls.log")))

    def test_rehearsing_a_draft_needs_the_draft_and_the_in_app_update(self):
        self.bundle(self.fix)
        self.assertEqual(self.release("cut", "--actions").returncode, 0)
        refused = self.release("rehearse", "0.2.4", "--from-draft")
        self.assertIn("v0.2.4 has no draft release to rehearse", refused.stderr)
        self.assertIn("drop --staged", self.release("rehearse", "0.2.4", "--from-draft", "--staged").stderr)
        self.assertEqual(self.log("rehearse.log"), [])

    def test_publish_in_actions_pushes_nothing_the_workflow_would_not_sign(self):
        self.bundle(self.fix)
        self.assertEqual(self.release("cut", "--actions").returncode, 0)
        (self.root / "admit-sign").write_text("false\n")
        result = self.release("publish", "0.2.4", "--actions", "--yes")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn(f"would not sign v0.2.4, so nothing is pushed; cut and sign it here: "
                      f"scripts/release cut --commit {self.fix} --key KEY", result.stderr)
        (self.root / "variables-fail").touch()
        self.assertIn("cannot read the repository variables", self.release("publish", "0.2.4", "--actions", "--yes").stderr)
        (self.root / "variables-fail").unlink()
        (self.root / "admit-refuses").touch()
        result = self.release("publish", "0.2.4", "--actions", "--yes")
        self.assertIn("refused by the fixture", result.stderr)
        self.assertIn("would refuse v0.2.4; nothing is pushed", result.stderr)
        self.assertEqual(self.git("tag", "--list", "v0.2.4", cwd=self.origin), "")

    def test_a_tag_pushed_but_never_published_is_cut_here_from_the_same_tag(self):
        self.bundle(self.fix)
        self.assertEqual(self.release("cut", "--actions").returncode, 0)
        self.git("push", "-q", "origin", "main", "v0.2.4")
        pushed = self.git("rev-parse", "v0.2.4", cwd=self.origin)
        # Another tag object of the same name here gives way to origin's.
        self.git("tag", "-d", "v0.2.4")
        self.git("tag", "-a", "v0.2.4", "-m", "another", self.fix)
        result = self.release("cut", "--commit", self.fix, "--key", str(self.root / "key"))
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("cutting it here, from the same tag", result.stdout)
        self.assertEqual(self.git("rev-parse", "v0.2.4"), pushed)
        self.assertTrue((self.root / "bundles/0.2.4").is_dir())

    def test_a_tag_pushed_at_another_commit_is_not_cut_again(self):
        self.bundle(self.feature)
        self.bundle(self.fix)
        self.git("push", "-q", "origin", "main")
        self.git("tag", "-a", "v0.2.4", "-m", "v0.2.4", self.feature)
        self.git("push", "-q", "origin", "v0.2.4")
        self.git("tag", "-d", "v0.2.4")
        result = self.release("cut", "--commit", self.fix, "--key", str(self.root / "key"))
        self.assertNotEqual(result.returncode, 0)
        self.assertIn(f"on origin at {self.feature} but not published", result.stderr)
        self.assertIn(f"scripts/release cut --commit {self.feature} --key KEY", result.stderr)


if __name__ == "__main__":
    unittest.main()
