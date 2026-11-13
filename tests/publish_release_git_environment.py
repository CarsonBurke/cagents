#!/usr/bin/env python3
"""Exercise the publisher's local-tag gate; never reach artifact smoke or gh."""
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[1]
PUBLISHER = ROOT / "scripts/publish-release"
VERSION = "1.2.3"
ROUTING = (
    "GIT_DIR", "GIT_WORK_TREE", "GIT_COMMON_DIR", "GIT_INDEX_FILE",
    "GIT_OBJECT_DIRECTORY", "GIT_ALTERNATE_OBJECT_DIRECTORIES",
)


class PublisherGitEnvironment(unittest.TestCase):
    def setUp(self):
        scratch = ROOT / ".worktrees"
        scratch.mkdir(exist_ok=True)
        temporary = tempfile.TemporaryDirectory(prefix="publisher-git-environment-", dir=scratch)
        self.addCleanup(temporary.cleanup)
        self.directory = Path(temporary.name)
        self.git = shutil.which("git")
        self.assertIsNotNone(self.git, "the regression requires real Git")
        self.environment = {
            key: value for key, value in os.environ.items() if not key.startswith("GIT_")
        }
        home = self.directory / "home"
        home.mkdir()
        self.environment.update(
            HOME=str(home), XDG_CONFIG_HOME=str(home / "config"),
            GIT_CONFIG_NOSYSTEM="1", GIT_CONFIG_SYSTEM=os.devnull,
            GIT_CONFIG_GLOBAL=os.devnull, GIT_TERMINAL_PROMPT="0",
        )
        self.checkout = self.repository("publisher checkout")
        self.foreign = self.repository("foreign checkout")
        scripts = self.checkout / "scripts"
        scripts.mkdir()
        self.publisher = scripts / "publish-release"
        self.publisher.write_bytes(PUBLISHER.read_bytes())
        self.publisher.chmod(0o755)
        self.guarded = self.directory / "unexpected-command"
        tools = self.directory / "tools"
        tools.mkdir()
        self.calls = self.directory / "git-calls.jsonl"
        # These paths must be unreachable: the deliberately absent dist fails
        # before mktemp, smoke, any network tool or a publication operation.
        guard = '#!/bin/sh\nprintf "%s\\n" "$0" >> "$PUBLISHER_GUARD"\nexit 98\n'
        for name in ("jq", "mktemp", "gh", "curl", "wget", "bwrap", "cargo", "rustc"):
            path = tools / name
            path.write_text(guard)
            path.chmod(0o755)
        (scripts / "smoke-release-artifacts").write_text(guard)
        (scripts / "smoke-release-artifacts").chmod(0o755)
        observer = tools / "git"
        observer.write_text(
            f"#!{sys.executable}\n"
            "import json,os,sys\n"
            "from pathlib import Path\n"
            "routing=" + repr(ROUTING) + "\n"
            "record=dict(routing_absent=not any(name in os.environ for name in routing),\n"
            " preserved=all(os.environ.get(key)==value for key,value in {\n"
            " 'PUBLISHER_ORDINARY':'ordinary-value',\n"
            " 'GIT_SSH_COMMAND':'synthetic-auth-command-never-run',\n"
            " 'GIT_CONFIG_COUNT':'1','GIT_CONFIG_KEY_0':'core.quotePath',\n"
            " 'GIT_CONFIG_VALUE_0':'false'}.items()))\n"
            "with Path(os.environ['PUBLISHER_GIT_CALLS']).open('a') as output:\n"
            " output.write(json.dumps(record)+'\\n')\n"
            "os.execv(os.environ['PUBLISHER_REAL_GIT'],['git',*sys.argv[1:]])\n"
        )
        observer.chmod(0o755)
        self.environment.update(
            PATH=f"{tools}:{self.environment['PATH']}",
            PUBLISHER_GUARD=str(self.guarded), PUBLISHER_GIT_CALLS=str(self.calls),
            PUBLISHER_REAL_GIT=self.git, PUBLISHER_ORDINARY="ordinary-value",
            GIT_SSH_COMMAND="synthetic-auth-command-never-run",
            GIT_CONFIG_COUNT="1", GIT_CONFIG_KEY_0="core.quotePath",
            GIT_CONFIG_VALUE_0="false",
        )
        self.missing_dist = self.directory / "absent release bundle"
        self.assertFalse(self.missing_dist.exists())

    def setup_git(self, repository, *arguments):
        return subprocess.run(
            [self.git, "-c", "core.hooksPath=/dev/null",
             "-c", "user.name=Fixture", "-c", "user.email=fixture@example.invalid",
             "-C", str(repository), *arguments],
            env=self.environment, capture_output=True, text=True, check=True, timeout=5,
        ).stdout.strip()

    def repository(self, name):
        repository = self.directory / name
        repository.mkdir()
        for relative in ("Cargo.toml", "crates/ceditor/Cargo.toml"):
            path = repository / relative
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(f'[package]\nname = "fixture"\nversion = "{VERSION}"\n')
        self.setup_git(repository, "init", "-q")
        self.setup_git(repository, "add", ".")
        self.setup_git(repository, "commit", "-qm", name)
        return repository

    @staticmethod
    def snapshot(repository):
        return {
            path.relative_to(repository).as_posix(): (path.stat().st_mode, path.read_bytes())
            for path in repository.rglob("*") if path.is_file()
        }

    def routed_environment(self):
        git = self.foreign / ".git"
        return {
            "GIT_DIR": str(git), "GIT_WORK_TREE": str(self.foreign),
            "GIT_COMMON_DIR": str(git), "GIT_INDEX_FILE": str(git / "index"),
            "GIT_OBJECT_DIRECTORY": str(git / "objects"),
            "GIT_ALTERNATE_OBJECT_DIRECTORIES": str(git / "objects"),
        }

    def invoke(self, routing=None):
        before = self.snapshot(self.foreign)
        index_before = (self.foreign / ".git/index").read_bytes()
        result = subprocess.run(
            [str(self.publisher), "--verify-only", "--dist", str(self.missing_dist)],
            cwd=self.directory, env=self.environment | (routing or {}),
            capture_output=True, text=True, timeout=10,
        )
        # Compare every owned foreign file and its mode, including the complete
        # index bytes, before asserting the tag diagnostic on either source.
        self.assertEqual(before, self.snapshot(self.foreign), "foreign repository changed")
        self.assertEqual(index_before, (self.foreign / ".git/index").read_bytes())
        self.assertFalse(self.guarded.exists(), "publisher crossed the invalid-dist boundary")
        self.assertFalse(self.missing_dist.exists())
        self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
        return result

    def assert_git_environment(self):
        calls = [json.loads(line) for line in self.calls.read_text().splitlines()]
        self.assertTrue(calls)
        self.assertTrue(all(call["routing_absent"] for call in calls))
        self.assertTrue(all(call["preserved"] for call in calls),
                        "ordinary, authentication or Git configuration environment changed")

    def test_foreign_tag_cannot_satisfy_missing_checkout_tag(self):
        self.setup_git(self.foreign, "tag", f"v{VERSION}")
        result = self.invoke(self.routed_environment())
        self.assertIn(f"publish-release: missing local commit tag: v{VERSION}", result.stderr)
        self.assertNotIn("missing release directory", result.stderr)
        self.assert_git_environment()

    def test_foreign_missing_tag_does_not_hide_checkout_tag(self):
        self.setup_git(self.checkout, "tag", f"v{VERSION}")
        result = self.invoke(self.routed_environment())
        self.assertEqual(result.stderr,
                         f"publish-release: missing release directory: {self.missing_dist}\n")
        self.assert_git_environment()

    def test_ordinary_checkout_tag_and_child_environment_are_preserved(self):
        self.setup_git(self.checkout, "tag", f"v{VERSION}")
        result = self.invoke()
        self.assertEqual(result.stderr,
                         f"publish-release: missing release directory: {self.missing_dist}\n")
        self.assert_git_environment()


if __name__ == "__main__":
    unittest.main()
