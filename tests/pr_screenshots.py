#!/usr/bin/python3
"""Contracts for scripts/pr-screenshots, with OCR and SSH stubbed and a local bare remote; no network."""
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import textwrap
import unittest

ROOT = Path(__file__).resolve().parents[1]
SCRIPT = ROOT / "scripts" / "pr-screenshots"
# Bounds only a hang; every run returns as soon as it finishes.
HANG_GUARD = 60
WEBP = b"RIFF\0\0\0\0WEBP\n"

STUBS = {
    # Reads an image's bytes as its text, as OCR reads the words on it. Here
    # it has no English data while NO_LOCAL_OCR is set; on the host it has.
    "tesseract": """
        #!/bin/sh
        if [ "$1" = --list-langs ]; then
            if [ -n "${NO_LOCAL_OCR:-}" ] && [ -z "${ON_HOST:-}" ]; then echo 'List of available languages (1):'; echo osd
            else echo 'List of available languages (1):'; echo eng; fi
            exit 0
        fi
        [ -z "${NO_LOCAL_OCR:-}" ] || [ -n "${ON_HOST:-}" ] || { echo 'Failed loading language eng' >&2; exit 1; }
        exec cat
    """,
    # Scaling for OCR passes the image through.
    "magick": """
        #!/bin/sh
        exec cat "$1"
    """,
    # Records what reaches the host, then runs it there.
    "ssh": """
        #!/bin/bash
        printf '%s\\n' "$*" >> "$STUB_LOG/ssh"
        while [ "$#" -gt 0 ]; do
            case $1 in
                -o) shift 2 ;;
                -*) shift ;;
                *) break ;;
            esac
        done
        [ "$1" = buildhost ] || { echo "ssh: unknown host $1" >&2; exit 255; }
        shift
        tee -a "$STUB_LOG/ssh-input" | ON_HOST=1 "$@"
    """,
}


class PrScreenshots(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory(prefix="pr-screenshots-")
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.logs = self.root / "logs"
        self.logs.mkdir()
        stubs = self.root / "bin"
        stubs.mkdir()
        for name, body in STUBS.items():
            (stubs / name).write_text(textwrap.dedent(body).lstrip())
            (stubs / name).chmod(0o755)
        self.config = self.root / "config"
        self.env = {
            "PATH": f"{stubs}{os.pathsep}{os.environ['PATH']}",
            "HOME": str(self.root / "home"),
            "XDG_CONFIG_HOME": str(self.config),
            "STUB_LOG": str(self.logs),
            "GIT_CONFIG_GLOBAL": "/dev/null",
            "GIT_CONFIG_NOSYSTEM": "1",
            # No real account registry; a test that needs accounts names its own.
            "CAGENTS_SCREENSHOT_CAGENTS": "/bin/false",
            "GIT_AUTHOR_NAME": "t", "GIT_AUTHOR_EMAIL": "t@example.invalid",
            "GIT_COMMITTER_NAME": "t", "GIT_COMMITTER_EMAIL": "t@example.invalid",
        }
        self.remote = self.root / "remote.git"
        self.git(self.root, "init", "-q", "--bare", str(self.remote))
        self.work = self.root / "work"
        self.git(self.root, "init", "-q", "--initial-branch=feature", str(self.work))
        (self.work / "README").write_text("product\n")
        self.git(self.work, "add", "README")
        self.git(self.work, "commit", "-qm", "product")
        self.git(self.work, "remote", "add", "origin", str(self.remote))
        self.shots = self.root / "task" / "shots"
        self.shots.mkdir(parents=True)

    def git(self, cwd, *arguments):
        return subprocess.run(["git", *arguments], cwd=cwd, env=self.env, check=True,
                              capture_output=True, text=True).stdout.strip()

    def image(self, name, text, directory=None):
        path = (directory or self.shots) / name
        path.write_bytes(WEBP + text.encode())
        return path

    def run_tool(self, *arguments, extra=None):
        return subprocess.run([str(SCRIPT), *map(str, arguments)], cwd=self.work, env={**self.env, **(extra or {})},
                              stdin=subprocess.DEVNULL, capture_output=True, text=True, timeout=HANG_GUARD)

    def remote_branches(self):
        return self.git(self.remote, "for-each-ref", "--format=%(refname)", "refs/heads/")

    def test_a_deliverable_address_is_refused_and_reported_masked(self):
        shown = self.image("after-account.webp", "Claude · someone.else@example.com  Opus 5.5")
        clean = self.image("before-account.webp", "Claude · fixture@example.test  codex@fixture.invalid")
        result = self.run_tool("scan", shown, clean)
        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertIn(f"{shown} shows an email address (s…e@e…e.com)", result.stderr)
        self.assertNotIn("someone.else", result.stderr)
        self.assertNotIn(str(clean), result.stderr)
        result = self.run_tool("scan", clean)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn(f"{clean}: no identity found", result.stdout)

    def test_the_git_email_and_private_identities_are_refused_despite_case_and_spacing(self):
        self.git(self.work, "config", "user.email", "dev@example.invalid")
        (self.config / "cagents").mkdir(parents=True)
        (self.config / "cagents" / "private-identities").write_text("# names that must not show\nJordan Example\n\n")
        git_email = self.image("after-git.webp", "Author dev@example.invalid")
        name = self.image("after-name.webp", "Signed in as JORDAN  EXAMPLE")
        result = self.run_tool("scan", git_email, name)
        self.assertEqual(result.returncode, 1)
        self.assertIn(f"{git_email} shows a private identity (d…d, from the Git email)", result.stderr)
        self.assertIn(f"{name} shows a private identity (J…e, from "
                      f"{self.config}/cagents/private-identities line 2)", result.stderr)
        self.assertNotIn("Jordan", result.stderr)
        extra = self.image("after-extra.webp", "Workspace of handle-42")
        self.assertEqual(self.run_tool("scan", "--identity", "handle-42", extra).returncode, 1)

    def test_addresses_ocr_misreads_are_still_found(self):
        for text in ("someone©example.com", "someone@®example.com", "someone ® example.com",
                     "someone @ example . com", "someone(a)example.com", "someone@example.c0m"):
            with self.subTest(text=text):
                result = self.run_tool("scan", self.image("after-misread.webp", f"Claude · {text}  Opus"))
                self.assertEqual(result.returncode, 1, result.stderr)
                self.assertIn("shows an email address (s…e@e…e.", result.stderr)

    def test_a_known_address_is_found_by_its_user_name_when_cut_off(self):
        self.git(self.work, "config", "user.email", "jordan.dev@example.invalid")
        result = self.run_tool("scan", self.image("after-cut.webp", "Signed in as jordan.dev@exam…"))
        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertIn("a private identity (j…v, from the Git email, before its @)", result.stderr)

    def test_the_addresses_of_cagents_accounts_are_refused(self):
        accounts = self.root / "cagents"
        accounts.write_text(textwrap.dedent("""\
            #!/bin/sh
            [ "$*" = "account list --json --no-start" ] || exit 2
            echo '{"command":"account.list","data":{"accounts":[{"id":"claude/system","name":"owner.account@example.invalid"},{"id":"agy/system","name":null},{"id":"x","name":"Work"}]}}'
        """))
        accounts.chmod(0o755)
        shown = self.image("after-composer.webp", "Opus 5.5 · owner.account@example.invalid  Work")
        result = self.run_tool("scan", shown, extra={"CAGENTS_SCREENSHOT_CAGENTS": str(accounts)})
        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertIn("a private identity (o…d, from a cagents account)", result.stderr)
        # A label that is no address is no identity.
        self.assertNotIn("W…k", result.stderr)
        # Without the registry, a fixture address is fine.
        self.assertEqual(self.run_tool("scan", shown).returncode, 0)

    def test_an_allowed_address_or_domain_passes(self):
        shown = self.image("after-support.webp", "Contact help@example.com or ops@mail.example.org")
        self.assertEqual(self.run_tool("scan", shown).returncode, 1)
        result = self.run_tool("scan", "--allow", "help@example.com", "--allow", "example.org", shown)
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_without_ocr_here_it_reads_on_the_host_and_keeps_identities_here(self):
        (self.config / "cagents").mkdir(parents=True)
        (self.config / "cagents" / "private-identities").write_text("Jordan Example\n")
        shown = self.image("after-name.webp", "Jordan Example")
        # By default it reads on the evidence host, unreachable here.
        missing = self.run_tool("scan", shown, extra={"NO_LOCAL_OCR": "1"})
        self.assertEqual(missing.returncode, 3)
        self.assertIn("tesseract on fw16 could not read", missing.stderr)
        self.assertIn("no tesseract with English data", missing.stderr)
        (self.logs / "ssh").unlink()
        (self.logs / "ssh-input").unlink(missing_ok=True)
        result = self.run_tool("scan", shown, extra={"NO_LOCAL_OCR": "1", "CAGENTS_EVIDENCE_REMOTE": "buildhost"})
        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertIn("shows a private identity", result.stderr)
        self.assertIn("-o BatchMode=yes", (self.logs / "ssh").read_text())
        self.assertIn("buildhost tesseract stdin stdout", (self.logs / "ssh").read_text())
        # Only the image went; the identities never leave.
        self.assertEqual((self.logs / "ssh-input").read_bytes(), shown.read_bytes())
        self.assertNotIn("Jordan", (self.logs / "ssh").read_text())
        broken = self.run_tool("scan", shown, extra={"NO_LOCAL_OCR": "1", "CAGENTS_EVIDENCE_REMOTE": "elsewhere"})
        self.assertEqual(broken.returncode, 3)

    def test_warn_reports_without_failing(self):
        shown = self.image("after-account.webp", "someone@example.com")
        result = self.run_tool("scan", "--warn", shown)
        self.assertEqual(result.returncode, 0)
        self.assertIn("native-evidence: warning:", result.stderr)
        self.assertIn("publish will refuse it", result.stderr)
        result = self.run_tool("scan", "--warn", shown, extra={"NO_LOCAL_OCR": "1"})
        self.assertEqual(result.returncode, 0)
        self.assertIn("did not check the shot for identities", result.stderr)

    def test_publish_commits_only_the_images_and_prints_immutable_pairs(self):
        before = self.image("before-channel-prs.webp", "Projects  fixture@example.test")
        after = self.image("after-channel-prs.webp", "Projects  Pull requests")
        head = self.git(self.work, "rev-parse", "HEAD")
        result = self.run_tool("publish", "-R", "owner/repo", before, after)
        self.assertEqual(result.returncode, 0, result.stderr)
        commit = self.git(self.remote, "rev-parse", "refs/heads/feature-screenshots")
        self.assertEqual(self.git(self.remote, "ls-tree", "-r", "--name-only", commit).splitlines(),
                         ["pr-screenshots/after-channel-prs.webp", "pr-screenshots/before-channel-prs.webp"])
        self.assertEqual(self.git(self.remote, "show", f"{commit}:pr-screenshots/after-channel-prs.webp"),
                         after.read_text())
        link = f"https://github.com/owner/repo/blob/{commit}/pr-screenshots/after-channel-prs.webp?raw=true"
        self.assertIn(f"after-channel-prs.webp\t{link}", result.stdout)
        self.assertIn(f"| Before | After |\n| --- | --- |\n| ![Before](https://github.com/owner/repo/blob/{commit}"
                      f"/pr-screenshots/before-channel-prs.webp?raw=true) | ![After]({link}) |\n\nchannel-prs",
                      result.stdout)
        self.assertEqual(self.git(self.remote, "log", "-1", "--format=%s", commit), "chore: add screenshots for feature")
        # The checkout, its index and its branch are left alone.
        self.assertEqual(self.git(self.work, "rev-parse", "HEAD"), head)
        self.assertEqual(self.git(self.work, "status", "--porcelain"), "")
        self.assertEqual(self.git(self.work, "branch", "--show-current"), "feature")
        # A later image goes on top, keeping the earlier ones reachable.
        later = self.image("after-empty.webp", "No projects yet")
        result = self.run_tool("publish", "-R", "owner/repo", "--branch", "feature", later)
        self.assertEqual(result.returncode, 0, result.stderr)
        tip = self.git(self.remote, "rev-parse", "refs/heads/feature-screenshots")
        self.assertEqual(self.git(self.remote, "rev-parse", f"{tip}^"), commit)
        self.assertEqual(len(self.git(self.remote, "ls-tree", "-r", "--name-only", tip).splitlines()), 3)

    def test_publish_refuses_everything_when_one_image_shows_an_identity(self):
        clean = self.image("before-account.webp", "fixture@example.test")
        shown = self.image("after-account.webp", "someone@example.com")
        result = self.run_tool("publish", "-R", "owner/repo", clean, shown)
        self.assertEqual(result.returncode, 1)
        self.assertIn("nothing was published", result.stderr)
        self.assertEqual(self.remote_branches(), "")
        # Masked, it goes.
        shown.write_bytes(WEBP + b"s\xe2\x96\x88\xe2\x96\x88\xe2\x96\x88@\xe2\x96\x88\xe2\x96\x88\xe2\x96\x88")
        result = self.run_tool("publish", "-R", "owner/repo", clean, shown)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.remote_branches(), "refs/heads/feature-screenshots")

    def test_publish_refuses_what_it_cannot_check(self):
        shot = self.image("after-home.webp", "Projects")
        result = self.run_tool("publish", "-R", "owner/repo", shot, extra={"NO_LOCAL_OCR": "1"})
        self.assertEqual(result.returncode, 3)
        self.assertIn("nothing was published", result.stderr)
        self.assertEqual(self.remote_branches(), "")

    def test_publish_refuses_links_to_another_repository_than_it_pushes_to(self):
        self.git(self.work, "remote", "add", "github", "git@github.com:owner/repo.git")
        shot = self.image("after-home.webp", "Projects")
        result = self.run_tool("publish", "--remote", "github", "-R", "someone/else", shot)
        self.assertEqual(result.returncode, 1)
        self.assertIn("-R someone/else is not github, which is owner/repo", result.stderr)

    def test_publish_refuses_raw_copies_other_formats_and_bad_names(self):
        raw = self.shots.parent / "raw"
        raw.mkdir()
        original = self.image("after-home.webp", "Projects", directory=raw)
        png = self.shots / "after-home.png"
        png.write_bytes(b"\x89PNG\r\n\x1a\nProjects")
        spaced = self.image("after home.webp", "Projects")
        result = self.run_tool("publish", "-R", "owner/repo", original, spaced)
        self.assertEqual(result.returncode, 2)
        self.assertIn(f"{original}: this is native-evidence's unedited copy; publish the one in shots/", result.stderr)
        self.assertIn(f"{spaced}: name it NAME.webp", result.stderr)
        disguised = self.shots / "after-home.webp"
        disguised.write_bytes(png.read_bytes())
        result = self.run_tool("publish", "-R", "owner/repo", disguised)
        self.assertEqual(result.returncode, 2)
        self.assertIn(f"{disguised}: not a WebP image", result.stderr)
        self.assertEqual(self.remote_branches(), "")


if __name__ == "__main__":
    unittest.main()
