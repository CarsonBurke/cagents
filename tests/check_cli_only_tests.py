"""Contracts for scripts/check-cli-only-tests."""

import importlib.machinery
import importlib.util
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
SCRIPT = ROOT / "scripts" / "check-cli-only-tests"
loader = importlib.machinery.SourceFileLoader("check_cli_only_tests", str(SCRIPT))
spec = importlib.util.spec_from_loader(loader.name, loader)
script = importlib.util.module_from_spec(spec)
loader.exec_module(script)

GATED = f"#[cfg({script.GATE})]\nmod tests {{\n    #[test]\n    fn works() {{}}\n}}\n"
UNGATED = "#[cfg(test)]\nmod tests {\n    #[test]\n    fn works() {}\n}\n"


class Tree(unittest.TestCase):
    def check(self, files):
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        root = Path(directory.name)
        for name, text in files.items():
            (root / name).parent.mkdir(parents=True, exist_ok=True)
            (root / name).write_text(text)
        return script.check(root)

    def assertRefused(self, files, module, reason):
        failures, _ = self.check(files)
        self.assertEqual(len(failures), 1, failures)
        self.assertTrue(failures[0].startswith(f"{module} runs only in the cli lane"), failures)
        self.assertIn(reason, failures[0])

    def test_this_repository_passes(self):
        failures, _ = script.check(ROOT)
        self.assertEqual(failures, [])

    def test_a_featureless_module_may_be_gated_and_is_not_listed_as_eligible(self):
        failures, eligible = self.check({
            "src/lib.rs": "mod plain;\nmod other;\n",
            "src/plain.rs": "pub fn plain() {}\n" + GATED,
            "src/other.rs": "pub fn other() {}\n" + UNGATED,
        })
        self.assertEqual(failures, [])
        self.assertEqual(eligible, ["src/other.rs:2: crate::other::tests"])

    def test_every_form_of_feature_branch_in_the_module_refuses_the_gate(self):
        for branch in ('#[cfg(feature = "host")]\nfn hosted() {}\n',
                       'fn f() -> bool { cfg!(feature="ui") }\n',
                       '#[cfg_attr(not(feature = "ui"), allow(dead_code))]\nfn f() {}\n'):
            with self.subTest(branch=branch):
                self.assertRefused({"src/lib.rs": "mod plain;\n", "src/plain.rs": branch + GATED},
                                   "crate::plain::tests", "src/plain.rs branches on a feature")

    def test_the_test_code_itself_is_scanned(self):
        tests = GATED.replace("fn works() {}", 'fn works() { assert!(cfg!(feature = "host")); }')
        self.assertRefused({"src/lib.rs": "mod plain;\n", "src/plain.rs": tests},
                           "crate::plain::tests", "src/plain.rs branches on a feature")

    def test_submodules_path_files_and_includes_of_the_owner_are_scanned(self):
        branch = '#[cfg(feature = "ui")]\nfn f() {}\n'
        cases = [
            ("src/plain/child.rs", {"src/lib.rs": "mod plain;\n", "src/plain.rs": "mod child;\n" + GATED,
                                   "src/plain/child.rs": branch}),
            ("src/plain/child.rs", {"src/lib.rs": "mod plain;\n", "src/plain/mod.rs": "mod child;\n" + GATED,
                                 "src/plain/child.rs": branch}),
            ("src/plain_tests.rs", {"src/lib.rs": "mod plain;\n",
                                   "src/plain.rs": f'#[cfg({script.GATE})]\n#[path = "plain_tests.rs"]\nmod tests;\n',
                                   "src/plain_tests.rs": branch}),
            ("src/plain/parts.rs", {"src/lib.rs": "mod plain;\n",
                                   "src/plain.rs": 'include!("plain/parts.rs");\n' + GATED,
                                   "src/plain/parts.rs": branch}),
            ("src/plain/inner/leaf.rs", {"src/lib.rs": "mod plain;\n",
                                        "src/plain.rs": "mod inner {\n    mod leaf;\n}\n" + GATED,
                                        "src/plain/inner/leaf.rs": branch}),
        ]
        for file, files in cases:
            with self.subTest(file=file):
                self.assertRefused(files, "crate::plain::tests", f"{file} branches on a feature")

    def test_a_module_the_cli_lane_does_not_build_refuses_the_gate(self):
        self.assertRefused({"src/lib.rs": '#[cfg(feature = "host")]\nmod hosted;\n',
                            "src/hosted.rs": "mod plain;\n", "src/hosted/plain.rs": GATED},
                           "crate::hosted::plain::tests", "crate::hosted is declared under a feature")

    def test_branches_outside_the_owner_and_in_comments_or_strings_elsewhere_do_not_count(self):
        failures, _ = self.check({
            "src/lib.rs": "mod plain;\nmod gated;\n",
            "src/gated.rs": '#[cfg(feature = "host")]\nfn f() {}\n',
            "src/plain.rs": '// mod fake;\nconst S: &str = "mod fake;";\n' + GATED,
        })
        self.assertEqual(failures, [])

    def test_an_unreadable_module_is_a_problem_not_a_pass(self):
        failures, _ = self.check({"src/lib.rs": "mod missing;\n"})
        self.assertEqual(len(failures), 1)
        self.assertIn("crate::missing: cannot read", failures[0])


if __name__ == "__main__":
    unittest.main()
