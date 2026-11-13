"""Contracts for scripts/script-contracts, the CI suite manifest and runner.

Synthetic suites and Git repositories live in private temporary roots; nothing
here executes another repository suite, Cargo or the network.
"""

import contextlib
import importlib.machinery
import importlib.util
import json
import os
import re
import shutil
import subprocess
from pathlib import Path
import tempfile
import unittest
import unittest.mock

ROOT = Path(__file__).resolve().parents[1]
SCRIPT = ROOT / "scripts/script-contracts"
loader = importlib.machinery.SourceFileLoader("script_contracts", str(SCRIPT))
spec = importlib.util.spec_from_loader(loader.name, loader)
contracts = importlib.util.module_from_spec(spec)
loader.exec_module(contracts)
Job, Suite = contracts.Job, contracts.Suite

# A fixture that records how it was started, then exits with EXIT.
RECORDING_FIXTURE = """\
import json, os, sys
from pathlib import Path
with Path("calls.jsonl").open("a") as output:
    output.write(json.dumps({{"fixture": {name!r}, "arguments": sys.argv[1:], "isolated": sys.flags.isolated,
                             "bytecode": sys.flags.dont_write_bytecode, "marker": os.environ.get("MARKER")}}) + "\\n")
raise SystemExit({exit_code})
"""

# Seven tests over two classes, one inherited from a mixin, each recording its
# id and whether its class setup ran first.
SHARDED_FIXTURE = """\
import unittest
from pathlib import Path


def record(test):
    with Path("ran.txt").open("a") as output:
        output.write(f"{test.id()} {getattr(type(test), 'prepared', False)}\\n")


class Mixin:
    def test_inherited(self):
        record(self)


class First(Mixin, unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.prepared = True

    def test_a(self): record(self)
    def test_b(self): record(self)
    def test_c(self): record(self)


class Second(unittest.TestCase):
    def test_d(self): record(self)
    def test_e(self): record(self)
    def test_f(self): record(self)


if __name__ == "__main__":
    unittest.main()
"""


@contextlib.contextmanager
def quiet():
    """Send this process's and its children's output to a scratch file."""
    with tempfile.TemporaryFile() as scratch:
        saved = os.dup(1), os.dup(2)
        try:
            os.dup2(scratch.fileno(), 1)
            os.dup2(scratch.fileno(), 2)
            yield
        finally:
            os.dup2(saved[0], 1)
            os.dup2(saved[1], 2)
            os.close(saved[0])
            os.close(saved[1])


class Tree(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory(prefix="script-contracts-")
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)

    def write(self, relative, text):
        path = self.root / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text)
        return path

    def recording(self, relative, name, exit_code=0):
        return self.write(relative, RECORDING_FIXTURE.format(name=name, exit_code=exit_code))

    def calls(self):
        path = self.root / "calls.jsonl"
        return [json.loads(line) for line in path.read_text().splitlines()] if path.exists() else []

    def run_job(self, jobs, name, shard=None):
        with quiet():
            return contracts.run(self.root, name, shard, jobs=jobs)

    def verify(self, jobs, **options):
        options.setdefault("unscheduled", ())
        options.setdefault("source_fixtures", ())
        options.setdefault("shell_syntax", ())
        return contracts.verify(self.root, jobs, **options)


class RepositoryManifest(unittest.TestCase):
    def test_the_repository_manifest_is_complete_and_consistent(self):
        self.assertEqual(contracts.verify(ROOT), [])

    def test_the_plan_covers_every_job_shard_once(self):
        entries = contracts.plan()
        expected = [(job.name, f"{index}/{job.shards}") for job in contracts.JOBS for index in range(1, job.shards + 1)]
        self.assertEqual([(entry["job"], entry["shard"]) for entry in entries], expected)
        self.assertEqual(len({entry["label"] for entry in entries}), len(entries))

    def test_the_workflow_tests_the_revision_it_was_defined_in(self):
        # A pull request's workflow comes from its merge result. Checking out
        # the head instead ran newer suite lists against older sources.
        workflow = (ROOT / ".github/workflows/script-contracts.yml").read_text()
        self.assertFalse(re.search(r"^\s*ref:", workflow, re.MULTILINE),
                         "every job must check out the event's own revision")
        for invocation in ("scripts/script-contracts verify", "scripts/script-contracts plan",
                           "scripts/script-contracts run"):
            self.assertTrue(invocation in workflow, f"the workflow must call {invocation}")


class Verification(Tree):
    def suite(self, **options):
        return Suite("example", "tests/example.py", sources=("scripts/example",), **options)

    def complete(self):
        self.recording("tests/example.py", "example")
        self.write("scripts/example", "#!/bin/sh\nexit 79\n")

    def test_a_complete_suite_verifies_without_executing_anything(self):
        self.complete()
        jobs = (Job("job", (self.suite(syntax=(("sh", "scripts/example"),)),)),)
        self.assertEqual(self.verify(jobs), [])
        self.assertEqual(self.calls(), [])

    def test_every_missing_member_is_reported_together(self):
        jobs = (Job("job", (self.suite(),)),)
        problems = self.verify(jobs)
        self.assertEqual(problems, ["scripts/example: missing", "tests/example.py: missing"])

    def test_symlinked_dangling_and_nonregular_members_are_refused(self):
        for kind in ("symlink", "dangling", "directory", "parent symlink"):
            with self.subTest(kind=kind):
                for child in self.root.iterdir():
                    if child.is_symlink() or child.is_file():
                        child.unlink()
                    else:
                        shutil.rmtree(child)
                self.complete()
                source = self.root / "scripts/example"
                if kind == "symlink":
                    self.write("elsewhere", "#!/bin/sh\n")
                    source.unlink()
                    source.symlink_to(self.root / "elsewhere")
                elif kind == "dangling":
                    source.unlink()
                    source.symlink_to(self.root / "absent")
                elif kind == "directory":
                    source.unlink()
                    source.mkdir()
                else:
                    (self.root / "scripts").rename(self.root / "real-scripts")
                    (self.root / "scripts").symlink_to(self.root / "real-scripts", target_is_directory=True)
                problems = self.verify((Job("job", (self.suite(),)),))
                self.assertEqual(len(problems), 1, problems)
                self.assertIn("never a symlink", problems[0])
                self.assertEqual(self.calls(), [])

    def test_shell_syntax_errors_are_refused(self):
        self.complete()
        self.write("scripts/example", "#!/bin/sh\nif\n")
        problems = self.verify((Job("job", (self.suite(syntax=(("sh", "scripts/example"),)),)),))
        self.assertEqual(len(problems), 1, problems)
        self.assertIn("scripts/example: sh syntax error", problems[0])

    def test_an_unlisted_unittest_suite_is_refused_until_scheduled_or_excluded(self):
        self.complete()
        self.write("tests/forgotten.py", "import unittest\nunittest.main(verbosity=2)\n")
        self.write("tests/qualification/also_forgotten.py", "from unittest import TestCase, main\nmain()\n")
        self.write("tests/fixtures/helper.py", "import unittest\nunittest.main()\n")
        self.write("tests/helper.py", "VALUE = 'mentions import unittest only in a string'\n")
        jobs = (Job("job", (self.suite(),)),)
        problems = self.verify(jobs)
        self.assertEqual(len(problems), 2, problems)
        self.assertIn("tests/forgotten.py: unlisted unittest suite", problems[0])
        self.assertIn("tests/qualification/also_forgotten.py: unlisted unittest suite", problems[1])
        unscheduled = ("tests/forgotten.py", "tests/qualification/also_forgotten.py")
        self.assertEqual(self.verify(jobs, unscheduled=unscheduled), [])

    def test_inconsistent_manifests_are_refused(self):
        one = Suite("one", "tests/one.py")
        two = Suite("two", "tests/two.py")
        for jobs, expected in (
            ((Job("job", (one, two), shards=2),), "Sharded job job must hold exactly one suite"),
            ((Job("job", (one,)), Job("job", (two,))), "Duplicate job name: job"),
            ((Job("a", (one,)), Job("b", (Suite("one", "tests/two.py"),))), "Duplicate suite name: one"),
            ((Job("job", (one,), shards=0),), "Job job needs at least one shard"),
            ((Job("job", ()),), "Job job has no suites"),
        ):
            with self.subTest(expected=expected):
                self.assertIn(expected, contracts.manifest_problems(jobs))


class Running(Tree):
    def test_a_suite_runs_isolated_verbose_from_the_root_with_its_environment(self):
        self.recording("tests/strict.py", "strict")
        self.recording("tests/loose.py", "loose")
        jobs = (Job("job", (Suite("strict", "tests/strict.py", env=(("MARKER", "set"),)),
                            Suite("loose", "tests/loose.py", isolated=False))),)
        self.assertEqual(self.run_job(jobs, "job"), 0)
        self.assertEqual(self.calls(), [
            {"fixture": "strict", "arguments": ["-v"], "isolated": 1, "bytecode": 1, "marker": "set"},
            {"fixture": "loose", "arguments": ["-v"], "isolated": 0, "bytecode": 1, "marker": None},
        ])

    def test_every_suite_runs_and_the_first_failure_status_is_preserved(self):
        self.recording("tests/one.py", "one", exit_code=19)
        self.recording("tests/two.py", "two", exit_code=0)
        self.recording("tests/three.py", "three", exit_code=7)
        jobs = (Job("job", tuple(Suite(name, f"tests/{name}.py") for name in ("one", "two", "three"))),)
        self.assertEqual(self.run_job(jobs, "job"), 19)
        self.assertEqual([call["fixture"] for call in self.calls()], ["one", "two", "three"])

    def test_a_job_with_a_broken_suite_executes_nothing(self):
        self.recording("tests/one.py", "one")
        self.write("scripts/two", "#!/bin/sh\nif\n")
        self.recording("tests/two.py", "two")
        jobs = (Job("job", (Suite("one", "tests/one.py"),
                            Suite("two", "tests/two.py", syntax=(("sh", "scripts/two"),)))),)
        with self.assertRaisesRegex(contracts.ContractError, "scripts/two: sh syntax error"):
            self.run_job(jobs, "job")
        self.assertEqual(self.calls(), [])

    def test_unknown_jobs_and_mismatched_shards_are_refused(self):
        self.recording("tests/one.py", "one")
        jobs = (Job("job", (Suite("one", "tests/one.py"),), shards=3),)
        for name, shard, message in (("other", None, "Unknown job"), ("job", "2/2", "does not match"),
                                     ("job", "4/3", "does not match"), ("job", "x", "must be I/N")):
            with self.subTest(name=name, shard=shard):
                with self.assertRaisesRegex(contracts.ContractError, message):
                    self.run_job(jobs, name, shard)
        self.assertEqual(self.calls(), [])


class Sharding(Tree):
    def ran(self):
        return (self.root / "ran.txt").read_text().splitlines()

    def test_shards_partition_the_suite_including_inherited_tests_and_class_setup(self):
        self.write("tests/many.py", SHARDED_FIXTURE)
        jobs = (Job("job", (Suite("many", "tests/many.py"),), shards=3),)
        for index in range(1, 4):
            before = len(self.ran()) if (self.root / "ran.txt").exists() else 0
            self.assertEqual(self.run_job(jobs, "job", f"{index}/3"), 0)
            self.assertIn(len(self.ran()) - before, (2, 3))
        ran = self.ran()
        self.assertEqual(sorted(line.split()[0] for line in ran), [
            "__main__.First.test_a", "__main__.First.test_b", "__main__.First.test_c",
            "__main__.First.test_inherited",
            "__main__.Second.test_d", "__main__.Second.test_e", "__main__.Second.test_f",
        ])
        self.assertTrue(all(line.endswith(" True") for line in ran if ".First." in line), ran)

    def test_a_failing_test_fails_only_its_shard(self):
        self.write("tests/many.py", SHARDED_FIXTURE.replace("def test_d(self): record(self)",
                                                            "def test_d(self): self.fail('broken')"))
        jobs = (Job("job", (Suite("many", "tests/many.py"),), shards=2),)
        statuses = [self.run_job(jobs, "job", f"{index}/2") for index in (1, 2)]
        self.assertEqual(sorted(statuses), [0, 1])

    def test_an_empty_shard_fails_rather_than_passing_vacuously(self):
        self.write("tests/one.py", "import unittest\n\nclass One(unittest.TestCase):\n"
                                   "    def test_only(self): pass\n\nunittest.main()\n")
        jobs = (Job("job", (Suite("one", "tests/one.py"),), shards=2),)
        self.assertEqual(self.run_job(jobs, "job", "1/2"), 0)
        self.assertEqual(self.run_job(jobs, "job", "2/2"), 2)

    def test_a_fixture_without_unittest_main_cannot_be_sharded(self):
        jobs = (Job("job", (Suite("plain", "tests/plain.py"),), shards=2),)
        for source, status in (("print('no tests')\n", 2), ("raise SystemExit(0)\n", 2),
                               ("raise SystemExit(5)\n", 5)):
            with self.subTest(source=source):
                self.write("tests/plain.py", source)
                self.assertEqual(self.run_job(jobs, "job", "1/2"), status)

    def test_sharded_and_whole_runs_see_the_same_file_and_import_path(self):
        self.write("tests/where.py", "import json, sys, unittest\nfrom pathlib import Path\n\n"
                                     "class Where(unittest.TestCase):\n"
                                     "    def test_where(self):\n"
                                     "        with Path('where.jsonl').open('a') as output:\n"
                                     "            output.write(json.dumps([__file__, sys.path[0]]) + '\\n')\n"
                                     "    def test_other(self): pass\n\nunittest.main()\n")
        for isolated in (True, False):
            with self.subTest(isolated=isolated):
                (self.root / "where.jsonl").unlink(missing_ok=True)
                suite = Suite("where", "tests/where.py", isolated=isolated)
                self.assertEqual(self.run_job((Job("whole", (suite,)),), "whole"), 0)
                for index in (1, 2):
                    self.assertEqual(self.run_job((Job("split", (suite,), shards=2),), "split", f"{index}/2"), 0)
                whole, sharded = [json.loads(line) for line in (self.root / "where.jsonl").read_text().splitlines()]
                self.assertEqual(sharded, whole)
                self.assertEqual(whole[0], str((self.root / "tests/where.py").resolve()))
                self.assertEqual(whole[1] == str((self.root / "tests").resolve()), not isolated)


class ProtocolBase(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory(prefix="script-contracts-git-")
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        environment = {"GIT_CONFIG_GLOBAL": os.devnull, "GIT_CONFIG_NOSYSTEM": "1",
                       "GIT_AUTHOR_NAME": "fixture", "GIT_AUTHOR_EMAIL": "fixture@invalid",
                       "GIT_COMMITTER_NAME": "fixture", "GIT_COMMITTER_EMAIL": "fixture@invalid"}
        patch = unittest.mock.patch.dict(os.environ, environment)
        patch.start()
        self.addCleanup(patch.stop)

    def git(self, repository, *arguments):
        return subprocess.run(["git", "-C", str(repository), *arguments], check=True,
                              capture_output=True, text=True).stdout.strip()

    def repository(self, name):
        repository = self.root / name
        repository.mkdir()
        self.git(repository, "init", "-q", "-b", "main")
        return repository

    def commit(self, repository, message):
        self.git(repository, "commit", "-q", "--allow-empty", "-m", message)
        return self.git(repository, "rev-parse", "HEAD")

    def test_a_pull_request_compares_its_merge_result_with_the_base_parent(self):
        repository = self.repository("merge")
        self.commit(repository, "root")
        self.git(repository, "checkout", "-q", "-b", "topic")
        self.commit(repository, "topic")
        self.git(repository, "checkout", "-q", "main")
        base = self.commit(repository, "main")
        self.git(repository, "merge", "-q", "--no-ff", "-m", "merge", "topic")
        self.assertEqual(contracts.protocol_base(repository, "pull_request"), base)
        self.git(repository, "checkout", "-q", "topic")
        with self.assertRaisesRegex(contracts.ContractError, "two-parent merge result"):
            contracts.protocol_base(repository, "pull_request")

    def test_a_push_requires_a_full_nonzero_object_name(self):
        repository = self.repository("push")
        before = self.commit(repository, "before")
        self.commit(repository, "after")
        self.assertEqual(contracts.protocol_base(repository, "push", before), before)
        for invalid in (None, "", "0" * 40, before[:12], before.upper(), before + "0"):
            with self.subTest(before=invalid):
                with self.assertRaisesRegex(contracts.ContractError, "trusted comparison base"):
                    contracts.protocol_base(repository, "push", invalid, fetch=False)
        with self.assertRaisesRegex(contracts.ContractError, "No protocol comparison base"):
            contracts.protocol_base(repository, "workflow_dispatch", before)

    def test_a_shallow_push_checkout_fetches_exactly_the_replaced_commit(self):
        origin = self.repository("origin")
        before = self.commit(origin, "before")
        self.commit(origin, "after")
        self.git(origin, "config", "uploadpack.allowAnySHA1InWant", "true")
        self.git(self.root, "clone", "-q", "--depth=1", f"file://{origin}", "clone")
        clone = self.root / "clone"
        self.assertEqual(contracts.protocol_base(clone, "push", before), before)
        self.git(clone, "cat-file", "-e", f"{before}^{{commit}}")
        with self.assertRaisesRegex(contracts.ContractError, "Cannot fetch"):
            contracts.protocol_base(clone, "push", "1" * 40)

if __name__ == "__main__":
    unittest.main()
