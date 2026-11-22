"""Contracts for scripts/release-metadata."""

import hashlib
import importlib.machinery
import importlib.util
import json
import os
import subprocess
import tempfile
import unittest
from pathlib import Path

SCRIPT = Path(__file__).resolve().parent.parent / "scripts" / "release-metadata"
loader = importlib.machinery.SourceFileLoader("release_metadata", str(SCRIPT))
spec = importlib.util.spec_from_loader(loader.name, loader)
metadata = importlib.util.module_from_spec(spec)
loader.exec_module(metadata)

VERSION = "1.2.3"
COMMIT = "a" * 40
ARCHIVE = f"cagents-{VERSION}-x86_64-unknown-linux-gnu.tar.gz"


def run(*args: str, check: bool = True) -> subprocess.CompletedProcess:
    return subprocess.run(
        ["python3", "-I", str(SCRIPT), *args], check=check, text=True,
        stdout=subprocess.PIPE, stderr=subprocess.PIPE,
    )


class ReleaseMetadata(unittest.TestCase):
    def setUp(self):
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        self.root = Path(directory.name)
        self.release = self.root / "release"
        self.release.mkdir()
        for name, content in ((ARCHIVE, b"archive"), ("install.sh", b"#!/bin/sh\n"), ("SHA256SUMS", b"sums\n")):
            (self.release / name).write_bytes(content)

    def write(self, *extra: str, check: bool = True) -> subprocess.CompletedProcess:
        return run("write", "--dir", str(self.release), "--version", VERSION,
                   "--source-commit", COMMIT, "--protocol", "196", *extra, check=check)

    def test_written_metadata_lists_every_file_and_checks(self):
        notes = self.root / "notes.md"
        notes.write_text("### New\n- Updates install with one click\n")
        printed = self.write("--notes", str(notes)).stdout.strip()
        raw = (self.release / "release-metadata.json").read_bytes()
        self.assertEqual(printed, hashlib.sha256(raw).hexdigest())
        written = json.loads(raw)
        self.assertEqual(written["schema_version"], 2)
        self.assertEqual(written["protocol"], 196)
        self.assertEqual(written["notes"], notes.read_text())
        self.assertEqual(written["assets"]["install.sh"],
                         {"size": 10, "sha256": hashlib.sha256(b"#!/bin/sh\n").hexdigest()})
        self.assertEqual(sorted(written["assets"]), sorted(["SHA256SUMS", ARCHIVE, "install.sh"]))
        self.assertEqual(written["targets"][0]["products"],
                         {"cagents": {"archive": ARCHIVE, "installer": "install.sh", "checksums": "SHA256SUMS"}})
        self.assertEqual(run("check", "--dir", str(self.release), "--version", VERSION,
                             "--source-commit", COMMIT).stdout.strip(), printed)

    def test_a_published_release_lists_every_product(self):
        for name in (f"cagents_{VERSION}_amd64.deb", f"cagents-browserless-{VERSION}-x86_64-unknown-linux-gnu.tar.gz",
                     "SHA256SUMS-browserless", f"ceditor-{VERSION}-x86_64-unknown-linux-gnu.tar.gz",
                     "SHA256SUMS-ceditor"):
            (self.release / name).write_bytes(b"payload")
        self.write()
        products = json.loads((self.release / "release-metadata.json").read_bytes())["targets"][0]["products"]
        self.assertEqual(sorted(products), ["cagents", "cagents-browserless", "ceditor"])
        self.assertEqual(products["cagents"]["deb"], f"cagents_{VERSION}_amd64.deb")
        self.assertNotIn("deb", products["cagents-browserless"])

    def test_check_refuses_a_changed_added_or_removed_file(self):
        self.write()
        for change, message in (
            (lambda: (self.release / "install.sh").write_bytes(b"#!/bin/sh\nevil\n"), "does not match"),
            (lambda: (self.release / "extra.txt").write_bytes(b"extra"), "extra.txt"),
            (lambda: (self.release / "SHA256SUMS").unlink(), "needs"),
        ):
            with self.subTest(message=message):
                saved = {path.name: path.read_bytes() for path in self.release.iterdir()}
                change()
                result = run("check", "--dir", str(self.release), check=False)
                self.assertEqual(result.returncode, 1)
                self.assertIn(message, result.stderr)
                for path in self.release.iterdir():
                    path.unlink()
                for name, content in saved.items():
                    (self.release / name).write_bytes(content)

    def test_check_refuses_another_version_commit_or_schema(self):
        self.write()
        self.assertIn("names version", run("check", "--dir", str(self.release), "--version", "1.2.4",
                                           check=False).stderr)
        self.assertIn("another source commit", run("check", "--dir", str(self.release), "--source-commit",
                                                   "b" * 40, check=False).stderr)
        path = self.release / "release-metadata.json"
        old = json.loads(path.read_bytes())
        old["schema_version"] = 1
        path.write_text(json.dumps(old))
        self.assertIn("not schema 2", run("check", "--dir", str(self.release), check=False).stderr)

    def test_write_refuses_bad_input(self):
        cases = (
            (("--version", "v1.2.3"), "stable semantic version"),
            (("--version", "1.2.3-rc.1"), "stable semantic version"),
            (("--source-commit", "abc"), "full lowercase SHA-1"),
            (("--protocol", "0"), "protocol"),
        )
        for override, message in cases:
            with self.subTest(message=message, override=override):
                arguments = {"--version": VERSION, "--source-commit": COMMIT, "--protocol": "196"}
                arguments[override[0]] = override[1]
                flat = [item for pair in arguments.items() for item in pair]
                result = run("write", "--dir", str(self.release), *flat, check=False)
                self.assertEqual(result.returncode, 1)
                self.assertIn(message, result.stderr)
        (self.release / "empty").write_bytes(b"")
        result = self.write(check=False)
        self.assertEqual(result.returncode, 1)
        self.assertIn("release file is empty: empty", result.stderr)

    def test_write_never_replaces_metadata(self):
        self.write()
        result = self.write(check=False)
        self.assertEqual(result.returncode, 1)
        self.assertIn("already exists", result.stderr)

    def test_notes_group_features_fixes_and_speedups(self):
        repository = self.root / "repository"
        environment = {**os.environ, "GIT_AUTHOR_NAME": "t", "GIT_AUTHOR_EMAIL": "t@example.invalid",
                       "GIT_COMMITTER_NAME": "t", "GIT_COMMITTER_EMAIL": "t@example.invalid"}

        def git(*args):
            subprocess.run(["git", "-C", str(repository), *args], check=True, env=environment,
                           stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)

        repository.mkdir()
        git("init", "-q")
        git("commit", "-q", "--allow-empty", "-m", "feat: the previous release")
        git("tag", "v1.2.2")
        for subject in ("chore: bump", "fix(chat): keep drafts across a restart (#2)",
                        "feat(update)!: update from the app (#1)", "perf: draw less (#3)", "docs: explain"):
            git("commit", "-q", "--allow-empty", "-m", subject)
        printed = run("notes", "--repository", str(repository), "--from", "v1.2.2", "--to", "HEAD").stdout
        self.assertEqual(printed, "### New\n- Update from the app (#1)\n\n### Fixed\n"
                                  "- Keep drafts across a restart (#2)\n\n### Faster\n- Draw less (#3)\n")

    def test_feed_offers_the_newest_signed_release(self):
        self.write()
        (self.release / "release-metadata.json.sig").write_text("signature")
        feed = self.root / "feed"
        self.assertIn("now offers 1.2.3", run("feed", "--dir", str(self.release), "--feed", str(feed)).stdout)
        latest = feed / "latest/download"
        self.assertEqual((latest / "release-metadata.json").read_bytes(),
                         (self.release / "release-metadata.json").read_bytes())
        self.assertEqual((feed / "download/v1.2.3" / ARCHIVE).read_bytes(), b"archive")
        self.assertIn("never replaces", run("feed", "--dir", str(self.release), "--feed", str(feed),
                                            check=False).stderr)
        # An older release joins the feed without becoming the latest.
        older = self.root / "older"
        older.mkdir()
        old_archive = ARCHIVE.replace(VERSION, "1.2.0")
        for name in (old_archive, "install.sh", "SHA256SUMS"):
            (older / name).write_bytes(b"old")
        run("write", "--dir", str(older), "--version", "1.2.0", "--source-commit", COMMIT, "--protocol", "190")
        (older / "release-metadata.json.sig").write_text("signature")
        self.assertIn("now offers 1.2.3", run("feed", "--dir", str(older), "--feed", str(feed)).stdout)
        self.assertEqual(json.loads((latest / "release-metadata.json").read_bytes())["version"], VERSION)
        self.assertTrue((feed / "download/v1.2.0" / old_archive).is_file())

    def test_feed_refuses_unsigned_metadata(self):
        self.write()
        result = run("feed", "--dir", str(self.release), "--feed", str(self.root / "feed"), check=False)
        self.assertEqual(result.returncode, 1)
        self.assertIn("sign the metadata first", result.stderr)


if __name__ == "__main__":
    unittest.main()
