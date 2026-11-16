"""Contracts for scripts/check-update-sources."""

import importlib.machinery
import importlib.util
import tempfile
import unittest
from pathlib import Path

SCRIPT = Path(__file__).resolve().parent.parent / "scripts" / "check-update-sources"
loader = importlib.machinery.SourceFileLoader("check_update_sources", str(SCRIPT))
spec = importlib.util.spec_from_loader(loader.name, loader)
checker = importlib.util.module_from_spec(spec)
loader.exec_module(checker)


class UpdateSourceDecisions(unittest.TestCase):
    def tree(self, protocol, released, retired, recordings):
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        root = Path(directory.name)
        (root / "src/terminal").mkdir(parents=True)
        (root / "src/terminal/ipc.rs").write_text(
            f"/// Docs.\npub const PROTOCOL_VERSION: u16 = {protocol};\n"
        )
        (root / "src/update").mkdir(parents=True)
        sources = "".join(
            f"    ReleasedSource {{\n        protocol: {value},\n"
            f'        release: "0.1.3+local.{value}",\n    }},\n'
            for value in released
        )
        # rustfmt wraps a long reason onto its own lines.
        reasons = "".join(
            f'    (\n        {value},\n        "a reason long enough to wrap",\n    ),\n'
            for value in retired
        )
        (root / "src/update/source.rs").write_text(
            "pub(crate) const RELEASED_SOURCES: &[ReleasedSource] = &[\n"
            f"{sources}];\n\n#[cfg(test)]\n"
            "pub(crate) const RETIRED_SOURCES: &[(u16, &str)] = &[\n"
            f"{reasons}];\n"
            "fn current() -> ReleasedSource { ReleasedSource { protocol: 999, } }\n"
        )
        (root / "tests/fixtures/wire-formats").mkdir(parents=True)
        for recording in recordings:
            (root / f"tests/fixtures/wire-formats/{recording}.json").write_text("{}\n")
        return root

    def test_every_served_protocol_pinned_or_retired_passes(self):
        root = self.tree(175, [156, 173], [160, 174], [128, 156, 160, 173, 174, 175])
        self.assertEqual(checker.problems(root), [])

    def test_a_bump_that_drops_the_installed_protocol_fails(self):
        # 174 then 175 replaced 173 without anyone deciding about it.
        (problem,) = checker.problems(self.tree(175, [156], [160, 174], [156, 160, 173, 174, 175]))
        self.assertIn("main served protocol 173", problem)

    def test_protocols_older_than_every_pinned_source_need_no_decision(self):
        self.assertEqual(checker.problems(self.tree(175, [156], [], [128, 129, 151, 156, 175])), [])

    def test_a_pinned_current_or_newer_protocol_fails(self):
        (problem,) = checker.problems(self.tree(175, [156, 175], [], [156, 175]))
        self.assertIn("pinned source protocol 175 is not older", problem)

    def test_a_pinned_protocol_without_a_recording_fails(self):
        (problem,) = checker.problems(self.tree(175, [156, 170], [], [156, 175]))
        self.assertIn("170 has no", problem)

    def test_a_protocol_both_pinned_and_retired_fails(self):
        (problem,) = checker.problems(self.tree(175, [156, 173], [173], [156, 173, 175]))
        self.assertIn("both pinned and retired", problem)

    def test_moved_source_lists_fail(self):
        root = self.tree(175, [156], [], [156, 175])
        (root / "src/update/source.rs").unlink()
        (problem,) = checker.problems(root)
        self.assertIn("source.rs not found", problem)


if __name__ == "__main__":
    unittest.main()
