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
            "GIT_DISCOVERY_ACROSS_FILESYSTEM", "GIT_ATTR_SOURCE",
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
        # This reads key NAMES before init/add/commit. Reject configuration
        # that can change with repository context or fetch missing objects.
        # Dormant filter definitions stay natural; actual attributes are
        # checked before seed conversion. Do not change HOME/config/authority.
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
            if name.startswith("includeif.")
            or (name.startswith("remote.") and name.endswith(".promisor"))
            or name == "extensions.partialclone"
        ]
        if unsupported:
            raise RuntimeError("fixture refuses external Git configuration names: " + ", ".join(unsupported))

    def git(self, repository, *arguments, check=True, input_data=None):
        result = subprocess.run(
            [
                "git", "--no-optional-locks", "-c", f"core.hooksPath={self.empty_hooks}",
                "-c", "core.fsmonitor=false", "-c", "commit.gpgSign=false",
                "-c", "tag.gpgSign=false", "-c", "user.name=Fixture",
                "-c", "user.email=fixture@example.invalid", "-C", str(repository),
                *arguments,
            ],
            capture_output=True, text=True, input=input_data, timeout=30,
        )
        if check:
            self.assertEqual(result.returncode, 0, result.stderr)
        return result

    def preflight_seed_attributes(self, repository):
        result = self.git(
            repository, "check-attr", "--all", "-z", "--", ".gitignore", "value.txt",
        )
        fields = result.stdout.split("\0")
        if fields.pop() != "" or len(fields) % 3:
            raise RuntimeError("cannot inspect fixture seed attributes")
        # --all omits truly unspecified attributes. Do not mistake a literal
        # filter=unset or filter=unspecified driver for a typed sentinel.
        if any(attribute == "filter" for attribute in fields[1::3]):
            raise RuntimeError("fixture refuses effective filter attributes before seed conversion")

    def configure_inert_filter(self, driver):
        names = self.git(self.repository, "config", "--name-only", "--list").stdout.splitlines()
        commands = {f"filter.{driver}.{suffix}" for suffix in ("clean", "smudge", "process")}
        if any(name.lower() in commands for name in names):
            raise RuntimeError("fixture refuses existing commands for its private test driver")
        # No executable is defined, even if a regression reaches conversion.
        self.git(self.repository, "config", "--local", f"filter.{driver}.required", "true")

    def prospective_filtered_commit(self, driver):
        attributes = self.root / f"attributes-{driver}"
        attributes.write_text(f"value.txt filter={driver}\n")
        blob = self.git(
            self.repository, "hash-object", "-w", "--no-filters", str(attributes),
        ).stdout.strip()
        entries = self.git(self.repository, "ls-tree", self.a).stdout
        tree = self.git(
            self.repository, "mktree",
            input_data=f"100644 blob {blob}\t.gitattributes\n" + entries,
        ).stdout.strip()
        return self.git(
            self.repository, "commit-tree", tree, "-p", self.a, "-m", "Fixture active filter",
        ).stdout.strip()

    def make_repository(self, repository):
        repository.mkdir()
        self.git(repository, "init", "-q", f"--template={self.empty_hooks}")
        (repository / ".gitignore").write_text("*.ignored\ncache/\n")
        (repository / "value.txt").write_text("commit A\n")
        self.preflight_seed_attributes(repository)
        self.git(repository, "add", ".gitignore", "value.txt")
        self.preflight_seed_attributes(repository)
        self.git(repository, "commit", "-q", "-m", "Fixture A")
        a = self.git(repository, "rev-parse", "HEAD").stdout.strip()
        (repository / "value.txt").write_text("commit B\n")
        self.preflight_seed_attributes(repository)
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

    def test_dormant_filter_definition_allows_exact_a_to_b(self):
        self.configure_inert_filter("validation-checkout-fixture")
        config_before = snapshot(self.common / "config")
        self.create()
        self.prepared(self.b, expected=self.a, operation="updated", previous=self.a)
        self.assertEqual((self.checkout / "value.txt").read_text(), "commit B\n")
        self.assertEqual(snapshot(self.common / "config"), config_before)

    def test_active_filters_are_refused_before_checkout_or_status(self):
        for driver in ("validation-checkout-fixture", "unset", "unspecified"):
            with self.subTest(driver=driver):
                self.configure_inert_filter(driver)
                head = self.prospective_filtered_commit(driver)
                fields = self.git(
                    self.repository, "check-attr", "--all", "-z", f"--source={head}", "--", "value.txt",
                ).stdout.split("\0")
                self.assertEqual(fields.pop(), "")
                self.assertEqual(len(fields) % 3, 0)
                self.assertIn(("value.txt", "filter", driver), list(zip(fields[::3], fields[1::3], fields[2::3])))
                source_before = snapshot(self.repository)
                self.assertFalse(os.path.lexists(self.checkout))
                self.assertFalse(os.path.lexists(self.record_path(self.checkout)))
                self.refused("invalid-input", head, expected="absent")
                self.assertEqual(snapshot(self.repository), source_before)
                self.assertFalse(os.path.lexists(self.checkout))
                self.assertFalse(os.path.lexists(self.record_path(self.checkout)))

        self.create()
        staged = self.root / "index-only-blob"
        staged.write_text("index-only staged content\n")
        blob = self.git(
            self.checkout, "hash-object", "-w", "--no-filters", str(staged),
        ).stdout.strip()
        self.git(self.checkout, "update-index", "--add", "--cacheinfo", f"100644,{blob},staged-only.txt")
        (self.checkout / "staged-only.txt").write_text("modified after index-only insertion\n")
        (self.checkout / ".gitattributes").write_text("staged-only.txt filter=validation-checkout-fixture\n")
        self.assertEqual(self.git(self.checkout, "ls-tree", "HEAD", "--", "staged-only.txt").stdout, "")
        self.assertEqual(
            self.git(self.checkout, "ls-files", "--error-unmatch", "--", "staged-only.txt").stdout,
            "staged-only.txt\n",
        )
        fields = self.git(
            self.checkout, "check-attr", "--all", "-z", "--", "staged-only.txt",
        ).stdout.split("\0")
        self.assertEqual(fields.pop(), "")
        self.assertEqual(len(fields) % 3, 0)
        self.assertIn(
            ("staged-only.txt", "filter", "validation-checkout-fixture"),
            list(zip(fields[::3], fields[1::3], fields[2::3])),
        )
        # Snapshot after deliberate fixture changes. No Git status is invoked
        # by this fixture: it could attempt clean conversion of the new path.
        source_before = snapshot(self.repository)
        self.refused("invalid-input")
        self.assertEqual(snapshot(self.repository), source_before)

    def test_conditional_include_is_refused_before_checkout_creation(self):
        included = self.root / "conditional-config"
        included.write_text("[user]\n\tname = Inert conditional fixture\n")
        # The source Git directory is not a linked-worktree admin directory.
        # A future checkout could activate this include in a new context.
        # Use only inert data, never a filter command or ambient config change.
        self.git(
            self.repository, "config", "--local",
            f"includeIf.gitdir:{self.common}/worktrees/**.path", str(included),
        )
        source_before = snapshot(self.repository)
        included_before = snapshot(included)
        self.assertFalse(os.path.lexists(self.checkout))
        self.assertFalse(os.path.lexists(self.record_path(self.checkout)))
        self.refused("invalid-input", head=self.a, expected="absent")
        self.assertEqual(snapshot(self.repository), source_before)
        self.assertEqual(snapshot(included), included_before)
        self.assertFalse(os.path.lexists(self.checkout))
        self.assertFalse(os.path.lexists(self.record_path(self.checkout)))

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

    def test_staged_gitlink_is_refused_before_populated_nested_status(self):
        self.create()
        nested = self.checkout / "nested-fixture"
        _, nested_head = self.make_repository(nested)
        self.git(self.checkout, "update-index", "--add", "--cacheinfo", f"160000,{nested_head},nested-fixture")
        (self.checkout / ".gitmodules").write_text(
            '[submodule "nested-fixture"]\n\tpath = nested-fixture\n',
        )
        self.assertEqual(self.git(self.checkout, "ls-tree", "HEAD", "--", "nested-fixture").stdout, "")
        self.assertEqual(
            self.git(self.checkout, "ls-files", "--stage", "--", "nested-fixture").stdout,
            f"160000 {nested_head} 0\tnested-fixture\n",
        )
        self.assertEqual(self.git(self.checkout, "ls-files", "-v", "--", "nested-fixture").stdout, "H nested-fixture\n")
        self.assertTrue((nested / ".git").is_dir())
        self.assertEqual(self.git(nested, "rev-parse", "HEAD").stdout.strip(), nested_head)
        names = self.git(nested, "config", "--name-only", "--list").stdout.splitlines()
        commands = {f"filter.validation-checkout-nested.{suffix}" for suffix in ("clean", "smudge", "process")}
        if any(name.lower() in commands for name in names):
            raise RuntimeError("fixture refuses existing commands for its private nested driver")
        self.git(nested, "config", "--local", "filter.validation-checkout-nested.required", "true")
        (nested / ".gitattributes").write_text("value.txt filter=validation-checkout-nested\n")
        (nested / "value.txt").write_text("modified nested tracked bytes\n")
        fields = self.git(nested, "check-attr", "--all", "-z", "--", "value.txt").stdout.split("\0")
        self.assertEqual(fields.pop(), "")
        self.assertEqual(len(fields) % 3, 0)
        self.assertIn(
            ("value.txt", "filter", "validation-checkout-nested"),
            list(zip(fields[::3], fields[1::3], fields[2::3])),
        )
        # No nested status or content conversion is used to establish the
        # control. This driver has no executable, even on a regression.
        source_before = snapshot(self.repository)
        nested_before = snapshot(nested)
        self.refused("invalid-input")
        self.assertEqual(snapshot(self.repository), source_before)
        self.assertEqual(snapshot(nested), nested_before)

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


class ScriptContractsApplicability(unittest.TestCase):
    """Run the exact CI block against private fake pairs, without Git or helpers."""

    pairs = (
        ("validation checkout", "scripts/prepare-validation-checkout", "tests/prepare_validation_checkout.py"),
        ("process custody", "scripts/qualification/process_custody.py", "tests/qualification/test_process_custody.py"),
    )

    @classmethod
    def setUpClass(cls):
        workflow = HELPER.parents[1] / ".github/workflows/script-contracts.yml"
        lines = workflow.read_text().splitlines(keepends=True)
        step = "      - name: Check validation checkout and process custody contracts\n"
        starts = [index for index, line in enumerate(lines) if line == step]
        if len(starts) != 1 or lines[starts[0] + 1] != "        run: |\n":
            raise AssertionError("expected exactly one inline applicability step")
        body = []
        for line in lines[starts[0] + 2:]:
            if line.startswith("          "):
                body.append(line[10:])
            elif not line.strip():
                body.append("\n")
            else:
                break
        cls.run_block = "".join(body)
        if not cls.run_block.startswith("python3 -I -B - <<'PY'\n"):
            raise AssertionError("applicability step must run its inline isolated Python")

    def setUp(self):
        temporary = tempfile.TemporaryDirectory(prefix="script-contract-applicability-", dir="/var/tmp")
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)

    def case(self, name):
        root = self.root / name
        root.mkdir(mode=0o700)
        return root

    def pair(self, root, index, *, exit_code=0):
        name, source, fixture = self.pairs[index]
        source = root / source
        fixture = root / fixture
        source.parent.mkdir(parents=True, exist_ok=True)
        fixture.parent.mkdir(parents=True, exist_ok=True)
        source.write_text("fixture source placeholder; never execute this file\n")
        fixture.write_text(
            "import json\nfrom pathlib import Path\nimport sys\n"
            f"entry = {{'fixture': {name!r}, 'arguments': sys.argv[1:], "
            "'isolated': sys.flags.isolated, 'bytecode': sys.flags.dont_write_bytecode}\n"
            "with Path('calls.jsonl').open('a') as output:\n"
            "    output.write(json.dumps(entry) + '\\n')\n"
            f"raise SystemExit({exit_code})\n"
        )

    def invoke_block(self, root):
        # The block is passed unchanged to a shell; the only executed Python
        # fixtures are those this test placed beneath the private cwd. No env
        # override, helper import, real Git repository or build is involved.
        return subprocess.run(
            ["/bin/sh", "-e", "-c", self.run_block], cwd=root,
            capture_output=True, text=True, timeout=15,
        )

    def calls(self, root):
        path = root / "calls.jsonl"
        return [json.loads(line) for line in path.read_text().splitlines()] if path.exists() else []

    def expected_calls(self, *indexes):
        return [
            {"fixture": self.pairs[index][0], "arguments": ["-v"], "isolated": 1, "bytecode": 1}
            for index in indexes
        ]

    def test_old_head_with_both_pairs_absent_is_explicitly_not_applicable(self):
        root = self.case("old-head")
        result = self.invoke_block(root)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout.splitlines(), [
            f"Not applicable: {name} (source and fixture both absent at this head)"
            for name, _, _ in self.pairs
        ])
        self.assertEqual(self.calls(root), [])
        self.assertEqual(list(root.iterdir()), [])

    def test_complete_pairs_run_in_order_with_existing_isolated_verbose_argv(self):
        root = self.case("current-head")
        self.pair(root, 0)
        self.pair(root, 1)
        result = self.invoke_block(root)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout.splitlines(), [f"Checking: {name}" for name, _, _ in self.pairs])
        self.assertEqual(self.calls(root), self.expected_calls(0, 1))

    def test_each_partial_pair_fails_instead_of_being_skipped(self):
        for index, (_, source, fixture) in enumerate(self.pairs):
            for missing in (source, fixture):
                with self.subTest(pair=index, missing=missing):
                    root = self.case(f"partial-{index}-{Path(missing).name}")
                    self.pair(root, index)
                    (root / missing).unlink()
                    result = self.invoke_block(root)
                    self.assertEqual(result.returncode, 2, result.stderr)
                    self.assertIn("Incomplete contract pair:", result.stderr)
                    self.assertEqual(self.calls(root), [])

    def test_each_pair_can_be_applicable_independently(self):
        for index in range(len(self.pairs)):
            with self.subTest(pair=index):
                root = self.case(f"independent-{index}")
                self.pair(root, index)
                result = self.invoke_block(root)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(self.calls(root), self.expected_calls(index))
                self.assertIn(f"Checking: {self.pairs[index][0]}\n", result.stdout)
                self.assertIn(f"Not applicable: {self.pairs[1 - index][0]} ", result.stdout)

    def test_original_failure_status_stops_later_pairs(self):
        for index, exit_code in ((0, 7), (1, 9)):
            with self.subTest(failing_pair=index):
                root = self.case(f"failure-{index}")
                self.pair(root, 0, exit_code=exit_code if index == 0 else 0)
                self.pair(root, 1, exit_code=exit_code if index == 1 else 0)
                result = self.invoke_block(root)
                self.assertEqual(result.returncode, exit_code, result.stderr)
                self.assertEqual(self.calls(root), self.expected_calls(*range(index + 1)))
                if index == 0:
                    self.assertNotIn("Checking: process custody", result.stdout)

    def test_symlink_and_dangling_pair_members_fail_without_following_them(self):
        for index, (_, source, fixture) in enumerate(self.pairs):
            for member_index, member in enumerate((source, fixture)):
                for dangling in (False, True):
                    with self.subTest(pair=index, member=member, dangling=dangling):
                        root = self.case(f"symlink-{index}-{member_index}-{dangling}")
                        self.pair(root, index)
                        path = root / member
                        target = root / "private-link-target"
                        if not dangling:
                            target.write_bytes(path.read_bytes())
                        path.unlink()
                        path.symlink_to(target)
                        result = self.invoke_block(root)
                        self.assertEqual(result.returncode, 2, result.stderr)
                        self.assertIn("Invalid contract path:", result.stderr)
                        self.assertTrue(path.is_symlink())
                        self.assertEqual(self.calls(root), [])

    def test_nonregular_members_and_parent_components_fail_closed(self):
        for index, (_, source, fixture) in enumerate(self.pairs):
            for member_index, member in enumerate((source, fixture)):
                with self.subTest(pair=index, directory_member=member):
                    root = self.case(f"directory-{index}-{member_index}")
                    self.pair(root, index)
                    path = root / member
                    path.unlink()
                    path.mkdir()
                    result = self.invoke_block(root)
                    self.assertEqual(result.returncode, 2, result.stderr)
                    self.assertIn("Invalid contract path:", result.stderr)
                    self.assertEqual(self.calls(root), [])
        for index, (_, source, fixture) in enumerate(self.pairs):
            for member_index, member in enumerate((source, fixture)):
                for kind in ("file", "symlink"):
                    with self.subTest(pair=index, parent_of=member, kind=kind):
                        root = self.case(f"parent-{index}-{member_index}-{kind}")
                        parent = root / Path(member).parts[0]
                        if kind == "file":
                            parent.write_text("non-directory fixture parent\n")
                        else:
                            target = root / "private-directory-target"
                            target.mkdir()
                            parent.symlink_to(target, target_is_directory=True)
                        result = self.invoke_block(root)
                        self.assertEqual(result.returncode, 2, result.stderr)
                        self.assertIn("Invalid contract path:", result.stderr)
                        self.assertEqual(self.calls(root), [])


if __name__ == "__main__":
    unittest.main()
