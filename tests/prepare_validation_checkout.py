#!/usr/bin/env python3
"""Black-box contracts for staging one stable validation checkout.

All Git repositories and intentional damage belong to each private fixture.
The helper receipt stages source; it supplies no lane or child-settlement proof.
"""

import hashlib
import json
import os
from pathlib import Path
import stat
import subprocess
import sys
import tempfile
import unittest


HELPER = Path(__file__).resolve().parents[1] / "scripts/prepare-validation-checkout"
OWNER = "fixture-lane-c"
SUCCESS_KEYS = {
    "version", "status", "operation", "repository", "common_git_dir", "checkout",
    "owner", "head", "tree", "previous_head", "detached", "clean", "registry",
    "source_only", "external_requirements",
}
REFUSAL_KEYS = {
    "version", "status", "code", "detail", "mutation_may_have_occurred",
}


def snapshot(path):
    """Preserve modes, symlinks and bytes without following directory links."""
    path = Path(path)
    if not os.path.lexists(path):
        return None
    mode = path.lstat().st_mode
    if stat.S_ISLNK(mode):
        return ("symlink", stat.S_IMODE(mode), os.readlink(path))
    if stat.S_ISDIR(mode):
        return (
            "directory", stat.S_IMODE(mode),
            tuple((entry.name, snapshot(entry)) for entry in sorted(path.iterdir())),
        )
    if stat.S_ISREG(mode):
        return ("file", stat.S_IMODE(mode), path.read_bytes())
    raise AssertionError(f"unexpected fixture entry kind: {path}")


class ValidationCheckout(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        # A fixture Git command must never be redirected into an ambient repo.
        # Inspect names only; retain the natural HOME and Agent authority.
        routing = {
            "GIT_DIR", "GIT_WORK_TREE", "GIT_COMMON_DIR", "GIT_INDEX_FILE",
            "GIT_OBJECT_DIRECTORY", "GIT_ALTERNATE_OBJECT_DIRECTORIES",
            "GIT_NAMESPACE", "GIT_CONFIG", "GIT_CONFIG_GLOBAL",
            "GIT_CONFIG_SYSTEM", "GIT_CONFIG_NOSYSTEM", "GIT_CONFIG_COUNT",
            "GIT_CONFIG_PARAMETERS", "GIT_CEILING_DIRECTORIES",
            "GIT_DISCOVERY_ACROSS_FILESYSTEM",
        }
        present = sorted(
            name for name in os.environ
            if name in routing or name.startswith(("GIT_CONFIG_KEY_", "GIT_CONFIG_VALUE_"))
        )
        if present:
            raise RuntimeError("fixture refuses inherited Git routing names: " + ", ".join(present))

    def setUp(self):
        temporary = tempfile.TemporaryDirectory(prefix="validation-checkout-contract-", dir="/var/tmp")
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.empty_hooks = self.root / "empty-template-and-hooks"
        self.empty_hooks.mkdir()
        self.repository = self.root / "repository"
        self.admitted_admin = {}
        self.preflight_git_only_context()
        self.a, self.b = self.make_repository(self.repository)
        common = self.git(self.repository, "rev-parse", "--git-common-dir").stdout.strip()
        self.common = (self.repository / common).resolve()
        self.checkout = self.root / "stable-validation"

    def preflight_git_only_context(self):
        # This reads key NAMES before init/add/commit. A filter or promisor
        # could otherwise execute during fixture seeding, before the helper's
        # own refusal. Do not change HOME/config/authority to make it pass.
        result = subprocess.run(
            ["git", "--no-optional-locks", "config", "--includes", "--name-only", "--list"],
            cwd=self.root, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
            text=True, timeout=30,
        )
        if result.returncode != 0:
            raise RuntimeError(f"cannot inspect Git configuration names (exit {result.returncode})")
        names = sorted(name.lower() for name in result.stdout.splitlines())
        unsupported = [
            name for name in names
            if name.startswith("filter.")
            or (name.startswith("remote.") and name.endswith(".promisor"))
            or name == "extensions.partialclone"
        ]
        if unsupported:
            raise RuntimeError("fixture refuses external Git configuration names: " + ", ".join(unsupported))

    def git(self, repository, *arguments, check=True):
        result = subprocess.run(
            [
                "git", "--no-optional-locks", "-c", f"core.hooksPath={self.empty_hooks}",
                "-c", "core.fsmonitor=false", "-c", "commit.gpgSign=false",
                "-c", "tag.gpgSign=false", "-c", "user.name=Fixture",
                "-c", "user.email=fixture@example.invalid", "-C", str(repository),
                *arguments,
            ],
            capture_output=True, text=True, timeout=30,
        )
        if check:
            self.assertEqual(result.returncode, 0, result.stderr)
        return result

    def make_repository(self, repository):
        repository.mkdir()
        self.git(repository, "init", "-q", f"--template={self.empty_hooks}")
        (repository / ".gitignore").write_text("*.ignored\ncache/\n")
        (repository / "value.txt").write_text("commit A\n")
        self.git(repository, "add", ".gitignore", "value.txt")
        self.git(repository, "commit", "-q", "-m", "Fixture A")
        a = self.git(repository, "rev-parse", "HEAD").stdout.strip()
        (repository / "value.txt").write_text("commit B\n")
        self.git(repository, "commit", "-q", "-a", "-m", "Fixture B")
        b = self.git(repository, "rev-parse", "HEAD").stdout.strip()
        self.assertNotEqual(a, b)
        return a, b

    def tree(self, head, repository=None):
        return self.git(repository or self.repository, "rev-parse", f"{head}^{{tree}}").stdout.strip()

    def invoke(self, head, *, expected, checkout=None, owner=OWNER, tree=None, repository=None):
        repository = repository or self.repository
        checkout = self.checkout if checkout is None else Path(checkout)
        return subprocess.run(
            [
                sys.executable, str(HELPER), "--repository", str(repository),
                "--checkout", str(checkout), "--owner", owner, "--head", head,
                "--tree", self.tree(head, repository) if tree is None else tree,
                "--expect-current", expected,
            ],
            cwd=self.root, capture_output=True, text=True, timeout=30,
        )

    def record_path(self, checkout):
        digest = hashlib.sha256(os.fsencode(checkout)).hexdigest()
        return self.common / "validation-checkouts-v1" / f"{digest}.json"

    def prepared(self, head, *, expected, operation, previous, checkout=None):
        checkout = self.checkout if checkout is None else checkout
        result = self.invoke(head, expected=expected, checkout=checkout)
        self.assertEqual(result.returncode, 0, result.stderr + result.stdout)
        receipt = json.loads(result.stdout)
        self.assertEqual(set(receipt), SUCCESS_KEYS)
        self.assertEqual(receipt["version"], 1)
        self.assertEqual(receipt["status"], "prepared")
        self.assertEqual(receipt["operation"], operation)
        self.assertEqual(receipt["repository"], str(self.repository.resolve()))
        self.assertEqual(receipt["common_git_dir"], str(self.common))
        self.assertEqual(receipt["checkout"], str(checkout))
        self.assertEqual(receipt["owner"], OWNER)
        self.assertEqual(receipt["head"], head)
        self.assertEqual(receipt["tree"], self.tree(head))
        self.assertEqual(receipt["previous_head"], previous)
        self.assertIs(receipt["detached"], True)
        self.assertIs(receipt["clean"], True)
        self.assertIs(receipt["source_only"], True)
        self.assertEqual(receipt["external_requirements"], [
            "exclusive_build_lane_ownership", "prior_owned_family_settled",
        ])
        self.assertEqual(self.git(checkout, "rev-parse", "HEAD").stdout.strip(), head)
        self.assertEqual(self.git(checkout, "rev-parse", "HEAD^{tree}").stdout.strip(), self.tree(head))
        self.assertEqual(self.git(checkout, "symbolic-ref", "-q", "HEAD", check=False).returncode, 1)
        self.assertEqual(self.git(checkout, "status", "--porcelain=v1", "--untracked-files=all", "--ignored").stdout, "")
        record = self.record_path(checkout)
        self.assertEqual(receipt["registry"], str(record))
        self.assertTrue(record.is_file())
        self.assertFalse(record.is_symlink())
        self.assertEqual(stat.S_IMODE(record.stat().st_mode), 0o600)
        self.assertEqual(record.stat().st_uid, os.geteuid())
        self.assertEqual(stat.S_IMODE(record.parent.stat().st_mode), 0o700)
        self.admitted_admin[str(checkout)] = Path(self.git(checkout, "rev-parse", "--absolute-git-dir").stdout.strip())
        return receipt

    def custody(self, checkout=None):
        checkout = self.checkout if checkout is None else checkout
        # Invocation runs in this private root. Never inspect a relative test
        # input against the caller's (possibly real repository) directory.
        if not checkout.is_absolute():
            checkout = self.root / checkout
        admin = self.admitted_admin.get(str(checkout))
        return (
            snapshot(checkout), snapshot(self.record_path(checkout)),
            snapshot(admin) if admin is not None else None,
            snapshot(self.common / "config"),
            self.git(self.repository, "worktree", "list", "--porcelain").stdout,
            self.git(self.repository, "rev-parse", "HEAD").stdout,
        )

    def refused(self, code, head=None, *, expected=None, checkout=None, **arguments):
        head = self.b if head is None else head
        expected = self.a if expected is None else expected
        checkout = self.checkout if checkout is None else checkout
        before = self.custody(checkout)
        result = self.invoke(head, expected=expected, checkout=checkout, **arguments)
        self.assertEqual(result.returncode, 2, result.stderr + result.stdout)
        receipt = json.loads(result.stdout)
        self.assertEqual(set(receipt), REFUSAL_KEYS)
        self.assertEqual(receipt["version"], 1)
        self.assertEqual(receipt["status"], "refused")
        self.assertEqual(receipt["code"], code)
        self.assertIsInstance(receipt["detail"], str)
        self.assertTrue(receipt["detail"])
        self.assertIs(receipt["mutation_may_have_occurred"], False)
        self.assertEqual(self.custody(checkout), before, "refusal changed owned fixture state")
        return receipt

    def create(self):
        return self.prepared(self.a, expected="absent", operation="created", previous=None)

    def test_creation_receipt_stages_exact_source_without_claiming_validation(self):
        self.create()
        self.assertEqual((self.checkout / "value.txt").read_text(), "commit A\n")
        self.assertEqual(self.git(self.repository, "rev-parse", "HEAD").stdout.strip(), self.b)

    def test_a_b_a_reuses_one_directory_and_admin_identity(self):
        first = self.create()
        identity = self.checkout.stat().st_dev, self.checkout.stat().st_ino
        admin = self.admitted_admin[str(self.checkout)]
        admin_identity = admin.stat().st_dev, admin.stat().st_ino
        git_file = (self.checkout / ".git").read_bytes()
        same = self.prepared(self.a, expected=self.a, operation="reused", previous=self.a)
        self.assertEqual(same["registry"], first["registry"])
        for head, prior in [(self.b, self.a), (self.a, self.b)]:
            receipt = self.prepared(head, expected=prior, operation="updated", previous=prior)
            self.assertEqual(receipt["registry"], first["registry"])
            self.assertEqual((self.checkout.stat().st_dev, self.checkout.stat().st_ino), identity)
            self.assertEqual((admin.stat().st_dev, admin.stat().st_ino), admin_identity)
            self.assertEqual((self.checkout / ".git").read_bytes(), git_file)
        inventory = self.git(self.repository, "worktree", "list", "--porcelain").stdout
        self.assertEqual(inventory.count(f"worktree {self.checkout}\n"), 1)

    def test_blob_replacement_cannot_substitute_the_staged_source(self):
        expected_tree = self.tree(self.a)
        original_blob = self.git(self.repository, "rev-parse", f"{self.a}:value.txt").stdout.strip()
        replacement = self.root / "replacement-blob.txt"
        replacement.write_text("replacement ref must not become validated source\n")
        replacement_blob = self.git(
            self.repository, "hash-object", "-w", "--no-filters", str(replacement),
        ).stdout.strip()
        self.git(self.repository, "replace", original_blob, replacement_blob)
        refs_before = self.git(
            self.repository, "for-each-ref", "--format=%(refname) %(objectname)", "refs/replace/",
        ).stdout
        ref_files_before = snapshot(self.common / "refs" / "replace")
        self.assertIn(f"refs/replace/{original_blob} {replacement_blob}", refs_before)
        self.assertEqual(
            self.git(self.repository, "show", f"{self.a}:value.txt").stdout,
            replacement.read_text(),
            "ordinary Git must actually observe the replacement for this regression",
        )
        result = self.invoke(self.a, expected="absent")
        self.assertEqual(result.returncode, 0, result.stderr + result.stdout)
        receipt = json.loads(result.stdout)
        self.assertEqual(set(receipt), SUCCESS_KEYS)
        self.assertEqual(receipt["status"], "prepared")
        self.assertEqual(receipt["head"], self.a)
        self.assertEqual(receipt["tree"], expected_tree)
        self.assertIs(receipt["source_only"], True)
        self.assertEqual((self.checkout / "value.txt").read_text(), "commit A\n")
        self.assertEqual(
            self.git(self.checkout, "--no-replace-objects", "rev-parse", "HEAD^{tree}").stdout.strip(),
            expected_tree,
        )
        self.assertEqual(
            self.git(self.checkout, "--no-replace-objects", "status", "--porcelain=v1", "--untracked-files=all", "--ignored").stdout,
            "",
        )
        self.assertEqual(self.git(
            self.repository, "for-each-ref", "--format=%(refname) %(objectname)", "refs/replace/",
        ).stdout, refs_before)
        self.assertEqual(snapshot(self.common / "refs" / "replace"), ref_files_before)

    def test_unstaged_bytes_are_refused_and_preserved(self):
        self.create()
        (self.checkout / "value.txt").write_text("private unstaged work\n")
        self.refused("dirty-checkout")

    def test_staged_bytes_and_index_are_refused_and_preserved(self):
        self.create()
        (self.checkout / "value.txt").write_text("private staged work\n")
        self.git(self.checkout, "add", "value.txt")
        self.refused("dirty-checkout")

    def test_untracked_file_is_refused_and_preserved(self):
        self.create()
        (self.checkout / "untracked.txt").write_bytes(b"private untracked work\x00")
        self.refused("dirty-checkout")

    def test_ignored_file_is_refused_and_preserved(self):
        self.create()
        (self.checkout / "artifact.ignored").write_bytes(b"private ignored artifact\x00")
        self.refused("dirty-checkout")

    def test_ignored_directory_is_refused_and_preserved(self):
        self.create()
        (self.checkout / "cache").mkdir()
        (self.checkout / "cache" / "generated.bin").write_bytes(b"private generated artifact\x00")
        self.refused("dirty-checkout")

    def test_hidden_index_flags_cannot_bless_modified_tracked_bytes(self):
        for flag, prefix in [("--assume-unchanged", "h "), ("--skip-worktree", "S ")]:
            with self.subTest(flag=flag):
                checkout = self.root / flag.removeprefix("--")
                self.prepared(self.a, expected="absent", operation="created", previous=None, checkout=checkout)
                self.git(checkout, "update-index", flag, "value.txt")
                (checkout / "value.txt").write_text("concealed private tracked changes\n")
                self.assertEqual(self.git(checkout, "status", "--porcelain=v1").stdout, "",
                                 "fixture must actually conceal the tracked modification")
                marked = self.git(checkout, "ls-files", "-v", "--", "value.txt").stdout
                self.assertTrue(marked.startswith(prefix), marked)
                self.refused("dirty-checkout", checkout=checkout)
                self.assertEqual(self.git(checkout, "ls-files", "-v", "--", "value.txt").stdout, marked)

    def test_sparse_checkout_is_not_repaired_or_labeled_clean(self):
        self.create()
        self.git(self.checkout, "sparse-checkout", "init", "--cone")
        self.assertEqual(self.git(self.checkout, "config", "--bool", "core.sparseCheckout").stdout.strip(), "true")
        self.refused("dirty-checkout")

    def test_candidate_gitlink_is_refused_without_attempting_submodule_setup(self):
        # A real Git tree contains a gitlink, but no clone/network or recursive
        # submodule helper is needed to establish the unsupported boundary.
        self.git(self.repository, "update-index", "--add", "--cacheinfo", f"160000,{self.a},nested-repository")
        self.git(self.repository, "commit", "-q", "-m", "Fixture unsupported gitlink")
        head = self.git(self.repository, "rev-parse", "HEAD").stdout.strip()
        self.refused("invalid-input", head, expected="absent")
        self.assertFalse(os.path.lexists(self.checkout))

    def test_existing_unregistered_directories_are_not_adopted(self):
        for occupied in [False, True]:
            with self.subTest(occupied=occupied):
                checkout = self.root / f"foreign-plain-{occupied}"
                checkout.mkdir()
                if occupied:
                    (checkout / "keep.txt").write_text("not owned by this registry\n")
                self.refused("foreign-worktree", self.a, expected="absent", checkout=checkout)

    def test_unregistered_detached_worktree_is_not_blessed(self):
        self.git(self.repository, "worktree", "add", "--quiet", "--detach", str(self.checkout), self.a)
        self.admitted_admin[str(self.checkout)] = Path(self.git(self.checkout, "rev-parse", "--absolute-git-dir").stdout.strip())
        self.refused("foreign-worktree")

    def test_registered_symbolic_branch_is_not_detached_or_repaired(self):
        self.create()
        self.git(self.checkout, "checkout", "-q", "-b", "owned-fixture-branch")
        self.refused("foreign-worktree")
        self.assertEqual(self.git(self.checkout, "symbolic-ref", "--short", "HEAD").stdout.strip(), "owned-fixture-branch")

    def test_symlinked_checkout_and_parent_are_never_followed(self):
        target = self.root / "foreign-target"
        target.mkdir()
        (target / "keep.txt").write_text("preserve target\n")
        self.checkout.symlink_to(target, target_is_directory=True)
        target_before = snapshot(target)
        self.refused("unsafe-path", self.a, expected="absent")
        self.assertEqual(snapshot(target), target_before)
        parent = self.root / "symlink-parent"
        parent.symlink_to(target, target_is_directory=True)
        self.refused("unsafe-path", self.a, expected="absent", checkout=parent / "nested-checkout")
        self.assertEqual(snapshot(target), target_before)

    def test_registered_checkout_rejects_another_owner_label(self):
        self.create()
        self.refused("foreign-owner", owner="another-owner")

    def test_registered_checkout_rejects_another_repository_common_dir(self):
        self.create()
        other = self.root / "other-repository"
        _, other_head = self.make_repository(other)
        self.refused("foreign-worktree", other_head, repository=other)

    def test_wrong_target_tree_is_rejected_before_creation_or_update(self):
        self.refused("tree-mismatch", self.a, expected="absent", tree=self.tree(self.b))
        self.assertFalse(os.path.lexists(self.checkout))
        self.create()
        self.refused("tree-mismatch", self.b, tree=self.tree(self.a))

    def test_stale_expectation_cannot_rewind_the_last_admitted_checkout(self):
        self.create()
        self.prepared(self.b, expected=self.a, operation="updated", previous=self.a)
        self.refused("unexpected-head", self.a, expected=self.a)
        self.assertEqual((self.checkout / "value.txt").read_text(), "commit B\n")

    def test_out_of_band_clean_head_is_not_a_new_registration(self):
        self.create()
        self.git(self.checkout, "checkout", "--quiet", "--detach", self.b)
        self.refused("unexpected-head", self.a, expected=self.b)

    def test_creation_requires_absent_and_reuse_requires_the_recorded_head(self):
        self.refused("unexpected-head", self.a, expected=self.a)
        self.assertFalse(os.path.lexists(self.checkout))
        self.create()
        self.refused("unexpected-head", self.b, expected="absent")

    def test_invalid_head_tree_owner_and_relative_checkout_preserve_registration(self):
        self.create()
        for code, arguments in [
            ("invalid-input", {"head": self.b[:12], "tree": self.tree(self.b)}),
            ("invalid-input", {"head": self.b, "tree": self.tree(self.b)[:12]}),
            ("invalid-input", {"head": self.b, "owner": "bad owner label"}),
            ("invalid-input", {"head": self.b, "expected": "HEAD"}),
            ("unsafe-path", {"head": self.b, "checkout": Path("relative-checkout")}),
        ]:
            with self.subTest(code=code, arguments=tuple(arguments)):
                self.refused(code, **arguments)


if __name__ == "__main__":
    unittest.main()
