#!/usr/bin/env python3
"""Exercise cache-qualifier Git isolation without Cargo or application builds."""

import hashlib
import importlib.util
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest
from unittest import mock


sys.dont_write_bytecode = True
ROOT = Path(__file__).resolve().parents[1]
SOURCE = ROOT / "scripts/qualify-checkout-cache.py"
SPEC = importlib.util.spec_from_file_location("checkout_cache_qualifier", SOURCE)
QUALIFIER = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(QUALIFIER)  # main(), and therefore Cargo, is never called.


class GitEnvironment(unittest.TestCase):
    def setUp(self):
        scratch = ROOT / ".worktrees"
        scratch.mkdir(exist_ok=True)
        temporary = tempfile.TemporaryDirectory(prefix="cache-git-environment-", dir=scratch)
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.git = shutil.which("git")
        self.assertIsNotNone(self.git, "the regression requires Git")
        self.environment = {
            key: value for key, value in os.environ.items() if not key.startswith("GIT_")
        }
        home = self.root / "home"
        home.mkdir()
        self.environment.update(
            HOME=str(home), XDG_CONFIG_HOME=str(home / "config"),
            GIT_CONFIG_NOSYSTEM="1", GIT_CONFIG_SYSTEM=os.devnull,
            GIT_CONFIG_GLOBAL=os.devnull, GIT_TERMINAL_PROMPT="0",
        )

    def setup_git(self, *arguments, cwd):
        return subprocess.run(
            [self.git, *arguments], cwd=cwd, env=self.environment,
            capture_output=True, text=True, check=True, timeout=5,
        ).stdout.strip()

    def assert_private_index(self, explicit_environment):
        foreign = self.root / "foreign"
        fixture = self.root / "fixture"
        foreign.mkdir()
        fixture.mkdir()
        (foreign / "foreign.txt").write_text("foreign index must remain unchanged\n")
        self.setup_git("init", "-q", cwd=foreign)
        self.setup_git("add", "foreign.txt", cwd=foreign)
        index = foreign / ".git/index"
        before = index.read_bytes()
        (fixture / "fixture.txt").write_text("owned fixture\n")
        environment = self.environment | {"GIT_INDEX_FILE": str(index)}
        with mock.patch.dict(os.environ, environment, clear=True):
            supplied = environment if explicit_environment else None
            QUALIFIER.run(self.git, "init", "-q", cwd=fixture, env=supplied)
            QUALIFIER.run(self.git, "add", "fixture.txt", cwd=fixture, env=supplied)
            self.assertEqual(os.environ["GIT_INDEX_FILE"], str(index))
            self.assertEqual(environment["GIT_INDEX_FILE"], str(index))
        after = index.read_bytes()
        self.assertEqual(
            before, after,
            "qualifier wrote the foreign index: "
            f"before={hashlib.sha256(before).hexdigest()} "
            f"after={hashlib.sha256(after).hexdigest()}",
        )
        self.assertEqual(self.setup_git("ls-files", cwd=fixture), "fixture.txt")
        self.assertTrue((fixture / ".git/index").is_file())

    def test_inherited_alternate_index_is_not_modified(self):
        self.assert_private_index(explicit_environment=False)

    def test_explicit_alternate_index_is_not_modified(self):
        self.assert_private_index(explicit_environment=True)

    def test_non_git_child_keeps_fixture_environment_without_repository_routing(self):
        environment = self.environment | {
            "GIT_DIR": str(self.root / "foreign.git"),
            "GIT_WORK_TREE": str(self.root / "foreign-tree"),
            "GIT_COMMON_DIR": str(self.root / "foreign-common"),
            "GIT_INDEX_FILE": str(self.root / "foreign-index"),
            "GIT_OBJECT_DIRECTORY": str(self.root / "foreign-objects"),
            "GIT_ALTERNATE_OBJECT_DIRECTORIES": str(self.root / "foreign-alternates"),
            "CAGENTS_CHECKOUT_ROOT": "deliberately-stale-inherited-value",
            "QUALIFICATION_PROBE_CHECKOUT": "owned-observer-checkout",
        }
        observed, _ = QUALIFIER.run(
            sys.executable, "-c",
            "import os\n"
            "valid = not any(os.environ.get(key) for key in "
            "('GIT_DIR','GIT_WORK_TREE','GIT_COMMON_DIR','GIT_INDEX_FILE',"
            "'GIT_OBJECT_DIRECTORY','GIT_ALTERNATE_OBJECT_DIRECTORIES'))\n"
            "valid = valid and os.environ['CAGENTS_CHECKOUT_ROOT']=='deliberately-stale-inherited-value'\n"
            "valid = valid and os.environ['QUALIFICATION_PROBE_CHECKOUT']=='owned-observer-checkout'\n"
            "valid = valid and os.environ['GIT_CONFIG_NOSYSTEM']=='1'\n"
            "print('fixture environment preserved' if valid else 'wrong fixture environment')",
            cwd=self.root, env=environment,
        )
        self.assertEqual(observed, "fixture environment preserved")


if __name__ == "__main__":
    unittest.main()
