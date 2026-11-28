"""Contracts for scripts/release-metadata."""

import hashlib
import importlib.machinery
import importlib.util
import json
import os
import shutil
import subprocess
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

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

        compact = json.dumps(written, separators=(",", ":")).encode()
        (self.release / "release-metadata.json").write_bytes(compact)
        self.assertEqual(run("check", "--dir", str(self.release)).stdout.strip(),
                         hashlib.sha256(compact).hexdigest())

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

    def assert_archive_requires_checksums(self, product, checksums):
        (self.release / f"{product}-{VERSION}-x86_64-unknown-linux-gnu.tar.gz").write_bytes(b"archive")
        path = self.release / "release-metadata.json"
        refused = self.write(check=False)
        self.assertEqual(refused.returncode, 1)
        self.assertIn(checksums, refused.stderr)
        self.assertFalse(path.exists())

        manifest = self.release / checksums
        manifest.write_bytes(b"checksums")
        self.write()
        stale = json.loads(path.read_bytes())
        del stale["assets"][checksums]
        path.write_text(json.dumps(stale))
        manifest.unlink()
        refused = run("check", "--dir", str(self.release), check=False)
        self.assertEqual(refused.returncode, 1)
        self.assertIn(checksums, refused.stderr)

    def test_a_browserless_archive_requires_its_checksum_file(self):
        self.assert_archive_requires_checksums("cagents-browserless", "SHA256SUMS-browserless")

    def test_a_ceditor_archive_requires_its_checksum_file(self):
        self.assert_archive_requires_checksums("ceditor", "SHA256SUMS-ceditor")

    def test_a_linux_release_names_no_macos_target(self):
        self.write()
        targets = json.loads((self.release / "release-metadata.json").read_bytes())["targets"]
        self.assertEqual([target["target"] for target in targets], ["x86_64-unknown-linux-gnu"])

    def test_a_release_with_the_macos_archive_names_its_target(self):
        darwin = f"cagents-{VERSION}-aarch64-apple-darwin.tar.gz"
        for name in (darwin, "install-macos.sh", "SHA256SUMS-macos"):
            (self.release / name).write_bytes(b"payload")
        self.write()
        written = json.loads((self.release / "release-metadata.json").read_bytes())
        linux, macos = written["targets"]
        self.assertEqual(linux["products"]["cagents"]["archive"], ARCHIVE)
        self.assertEqual(macos, {
            "target": "aarch64-apple-darwin", "os": "macos", "architecture": "aarch64",
            "products": {"cagents": {"archive": darwin, "installer": "install-macos.sh",
                                     "checksums": "SHA256SUMS-macos"}},
        })
        self.assertIn(darwin, written["assets"])
        self.assertEqual(run("check", "--dir", str(self.release)).returncode, 0)

    def test_a_partial_macos_release_is_refused(self):
        (self.release / f"cagents-{VERSION}-aarch64-apple-darwin.tar.gz").write_bytes(b"payload")
        refused = self.write(check=False)
        self.assertNotEqual(refused.returncode, 0)
        self.assertIn("install-macos.sh", refused.stderr)
        self.assertFalse((self.release / "release-metadata.json").exists())

    def test_android_assets_include_both_names_and_the_checksums(self):
        apk, alias = f"cagents-android-{VERSION}.apk", "cagents-android.apk"
        for name in (apk, alias):
            (self.release / name).write_bytes(b"fixture APK")
        sha = hashlib.sha256(b"fixture APK").hexdigest()
        checksums = self.release / "SHA256SUMS-android"
        checksums.write_text(f"{sha}  {apk}\n{sha}  {alias}\n")
        self.write()
        assets = json.loads((self.release / "release-metadata.json").read_bytes())["assets"]
        self.assertEqual(assets[apk], assets[alias])
        self.assertEqual(assets[apk]["sha256"], sha)
        self.assertIn(checksums.name, assets)
        run("check", "--dir", str(self.release))

    def test_android_assets_refuse_a_partial_wrong_version_or_mismatched_set(self):
        apk, alias = f"cagents-android-{VERSION}.apk", "cagents-android.apk"
        (self.release / alias).write_bytes(b"wrong alias")
        self.assertIn(apk, self.write(check=False).stderr)
        (self.release / f"cagents-android-1.2.2.apk").write_bytes(b"wrong version")
        self.assertIn(apk, self.write(check=False).stderr)
        (self.release / "cagents-android-1.2.2.apk").unlink()
        (self.release / apk).write_bytes(b"fixture APK")
        (self.release / "SHA256SUMS-android").write_text("invalid\n")
        self.assertIn("differs from", self.write(check=False).stderr)
        (self.release / alias).write_bytes(b"fixture APK")
        self.assertIn("does not match the two APKs", self.write(check=False).stderr)
        self.assertFalse((self.release / "release-metadata.json").exists())

    def test_a_linux_appimage_needs_its_identical_versioned_and_stable_names(self):
        image = self.release / f"cagents-{VERSION}-x86_64.AppImage"
        alias = self.release / "cagents-x86_64.AppImage"
        image.write_bytes(b"AppImage fixture")
        refused = self.write(check=False)
        self.assertEqual(refused.returncode, 1)
        self.assertIn(alias.name, refused.stderr)

        alias.write_bytes(b"another image")
        refused = self.write(check=False)
        self.assertEqual(refused.returncode, 1)
        self.assertIn("differs from the versioned AppImage", refused.stderr)

        alias.write_bytes(image.read_bytes())
        self.write()
        written = json.loads((self.release / "release-metadata.json").read_bytes())
        self.assertEqual(written["assets"][image.name], written["assets"][alias.name])
        self.assertEqual(written["targets"][0]["products"]["cagents"]["archive"], ARCHIVE)
        self.assertEqual(run("check", "--dir", str(self.release)).returncode, 0)

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

    def test_check_rejects_float_and_boolean_encodings_for_integer_fields(self):
        (self.release / ARCHIVE).write_bytes(b"x")
        self.write()
        path = self.release / "release-metadata.json"
        raw = path.read_bytes()
        for field, value in (("schema_version", 2.0), ("size", 1.0), ("size", True)):
            with self.subTest(field=field, value=value):
                metadata = json.loads(raw)
                target = metadata if field == "schema_version" else metadata["assets"][ARCHIVE]
                target[field] = value
                path.write_text(json.dumps(metadata))
                result = run("check", "--dir", str(self.release), check=False)
                self.assertEqual(result.returncode, 1)

    def test_check_rejects_duplicate_json_fields(self):
        self.write()
        path = self.release / "release-metadata.json"
        raw = path.read_text()
        duplicate = raw.replace('"schema_version": 2,', '"schema_version": 2,\n  "schema_version": 2,', 1)
        self.assertNotEqual(duplicate, raw)
        path.write_text(duplicate)
        result = run("check", "--dir", str(self.release), check=False)
        self.assertEqual(result.returncode, 1)
        self.assertIn("duplicate JSON field", result.stderr)

    def test_check_requires_utf8_json_without_a_byte_order_mark(self):
        self.write()
        path = self.release / "release-metadata.json"
        raw = path.read_text(encoding="utf-8")
        for encoding in ("utf-8-sig", "utf-16", "utf-32"):
            with self.subTest(encoding=encoding):
                path.write_bytes(raw.encode(encoding))
                result = run("check", "--dir", str(self.release), check=False)
                self.assertEqual(result.returncode, 1)
                self.assertIn("not UTF-8 JSON", result.stderr)

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
        for name in ("release-metadata.json", "release-metadata-2.json"):
            self.assertEqual((latest / name).read_bytes(),
                             (self.release / "release-metadata.json").read_bytes())
            self.assertEqual((feed / "download/v1.2.3" / name).read_bytes(),
                             (self.release / "release-metadata.json").read_bytes())
        self.assertEqual((latest / "release-metadata-2.json.sig").read_text(), "signature")
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

    def test_feed_after_the_bridge_release_keeps_its_metadata_under_the_old_names(self):
        bridge = self.root / "bridge"
        bridge.mkdir()
        bridge_archive = ARCHIVE.replace(VERSION, "1.2.0")
        for name in (bridge_archive, "install.sh", "SHA256SUMS"):
            (bridge / name).write_bytes(b"bridge")
        run("write", "--dir", str(bridge), "--version", "1.2.0", "--source-commit", COMMIT, "--protocol", "190")
        (bridge / "release-metadata.json.sig").write_text("old key")
        self.write()
        (self.release / "release-metadata.json.sig").write_text("new key")
        feed = self.root / "feed"
        run("feed", "--dir", str(self.release), "--feed", str(feed), "--bridge", str(bridge))
        for directory in (feed / "latest/download", feed / "download/v1.2.3"):
            self.assertEqual(json.loads((directory / "release-metadata-2.json").read_bytes())["version"], VERSION)
            self.assertEqual((directory / "release-metadata-2.json.sig").read_text(), "new key")
            self.assertEqual((directory / "release-metadata.json").read_bytes(),
                             (bridge / "release-metadata.json").read_bytes())
            self.assertEqual((directory / "release-metadata.json.sig").read_text(), "old key")
        # A bridge is always older than the release that carries it.
        result = run("feed", "--dir", str(bridge), "--feed", str(self.root / "other"),
                     "--bridge", str(self.release), check=False)
        self.assertIn("not older than", result.stderr)

    def test_feed_retry_advances_latest_without_replacing_the_tagged_release(self):
        older = self.root / "older"
        older.mkdir()
        for name in (ARCHIVE.replace(VERSION, "1.2.2"), "install.sh", "SHA256SUMS"):
            (older / name).write_bytes(b"old")
        run("write", "--dir", str(older), "--version", "1.2.2", "--source-commit", COMMIT, "--protocol", "196")
        (older / metadata.SIGNATURE).write_text("old signature")
        self.write()
        (self.release / metadata.SIGNATURE).write_text("new signature")
        for legacy in (False, True):
            with self.subTest(legacy=legacy):
                feed = self.root / str(legacy)
                run("feed", "--dir", str(older), "--feed", str(feed))
                latest = feed / "latest/download"
                if legacy and latest.is_symlink():
                    tagged_old = latest.resolve()
                    latest.unlink()
                    latest.mkdir()
                    for path in tagged_old.glob("release-metadata*"):
                        shutil.copyfile(path, latest / path.name)
                tagged = feed / f"download/v{VERSION}"
                fail_next_move = True

                def fail_before_selection(move):
                    def interrupted(source, destination):
                        nonlocal fail_next_move
                        if fail_next_move and destination == latest:
                            fail_next_move = False
                            raise OSError("fixture interrupted latest selection")
                        return move(source, destination)
                    return interrupted

                with (patch.object(Path, "rename", fail_before_selection(Path.rename)),
                      patch.object(Path, "replace", fail_before_selection(Path.replace)),
                      self.assertRaisesRegex(OSError, "interrupted latest")):
                    metadata.feed(self.release, feed, 2, None)

                inode = tagged.stat().st_ino
                self.assertEqual((tagged / ARCHIVE).read_bytes(), b"archive")
                old_latest = (latest / metadata.METADATA).read_bytes() if latest.is_dir() else None
                result = run("feed", "--dir", str(self.release), "--feed", str(feed), check=False)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(tagged.stat().st_ino, inode)
                self.assertEqual(old_latest, (older / metadata.METADATA).read_bytes())
                self.assertEqual((latest / metadata.METADATA).read_bytes(),
                                 (self.release / metadata.METADATA).read_bytes())

    def test_feed_retry_repairs_missing_latest_only_for_a_matching_tagged_release(self):
        self.write()
        (self.release / metadata.SIGNATURE).write_text("signature")
        feed = self.root / "feed"
        run("feed", "--dir", str(self.release), "--feed", str(feed))
        latest = feed / "latest/download"
        if latest.is_symlink():
            latest.unlink()
        else:
            shutil.rmtree(latest)
        tagged_archive = feed / f"download/v{VERSION}" / ARCHIVE
        tagged_archive.write_bytes(b"different archive")
        refused = run("feed", "--dir", str(self.release), "--feed", str(feed), check=False)
        self.assertIn("never replaces", refused.stderr)
        self.assertEqual(tagged_archive.read_bytes(), b"different archive")
        self.assertFalse(latest.exists())
        tagged_archive.write_bytes(b"archive")
        run("feed", "--dir", str(self.release), "--feed", str(feed))
        self.assertEqual((latest / metadata.METADATA).read_bytes(), (self.release / metadata.METADATA).read_bytes())

    def test_feed_supports_a_latest_directory_reached_by_symlink(self):
        self.write()
        (self.release / metadata.SIGNATURE).write_text("signature")
        feed = self.root / "feed"
        feed.mkdir()
        latest = self.root / "elsewhere/latest"
        latest.mkdir(parents=True)
        (feed / "latest").symlink_to(latest, target_is_directory=True)
        run("feed", "--dir", str(self.release), "--feed", str(feed))
        self.assertEqual((latest / "download" / metadata.METADATA).read_bytes(),
                         (self.release / metadata.METADATA).read_bytes())

    def test_feed_refuses_unsigned_metadata(self):
        self.write()
        result = run("feed", "--dir", str(self.release), "--feed", str(self.root / "feed"), check=False)
        self.assertEqual(result.returncode, 1)
        self.assertIn("sign the metadata first", result.stderr)


if __name__ == "__main__":
    unittest.main()
