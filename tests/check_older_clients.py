"""Contracts for scripts/check-older-clients."""

import importlib.machinery
import importlib.util
import tempfile
import unittest
from pathlib import Path

SCRIPT = Path(__file__).resolve().parent.parent / "scripts" / "check-older-clients"
loader = importlib.machinery.SourceFileLoader("check_older_clients", str(SCRIPT))
spec = importlib.util.spec_from_loader(loader.name, loader)
checker = importlib.util.module_from_spec(spec)
loader.exec_module(checker)


class OlderClientProof(unittest.TestCase):
    def tree(self, protocol, proven, recordings):
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        root = Path(directory.name)
        (root / "src/terminal/ipc").mkdir(parents=True)
        (root / "src/terminal/ipc.rs").write_text(
            f"/// Docs.\npub const PROTOCOL_VERSION: u16 = {protocol};\n"
        )
        clients = "".join(
            f"const C{index}: OlderClient = OlderClient {{\n    proven_at: {value},\n}};\n"
            for index, value in enumerate(proven)
        )
        clients += "const OLDER_CLIENTS: &[OlderClient] = &[" + ", ".join(
            f"C{index}" for index in range(len(proven))
        ) + "];\n"
        # A test fixture's symbolic value is not a claim about the release.
        clients += "fn knowing() -> OlderClient { OlderClient { proven_at: PROTOCOL_VERSION, } }\n"
        (root / "src/terminal/ipc/older_clients.rs").write_text(clients)
        (root / "tests/fixtures/wire-formats").mkdir(parents=True)
        for recording in recordings:
            (root / f"tests/fixtures/wire-formats/{recording}.json").write_text("{}\n")
        return root

    def test_clients_proven_at_the_recorded_protocol_pass(self):
        self.assertEqual(checker.problems(self.tree(151, [151], [128, 129, 151])), [])

    def test_an_explicitly_empty_list_passes_with_the_current_recording(self):
        self.assertEqual(checker.problems(self.tree(161, [], [160, 161])), [])

    def test_a_missing_list_fails_even_with_a_literal_proof(self):
        root = self.tree(151, [151], [151])
        path = root / "src/terminal/ipc/older_clients.rs"
        path.write_text(path.read_text().replace(
            "const OLDER_CLIENTS: &[OlderClient] = &[C0];\n", ""
        ))
        (problem,) = checker.problems(root)
        self.assertIn("expected one literal OLDER_CLIENTS list", problem)

    def test_a_nonempty_list_with_a_missing_claim_fails(self):
        root = self.tree(151, [151], [151])
        path = root / "src/terminal/ipc/older_clients.rs"
        path.write_text(path.read_text().replace("&[C0];", "&[C0, UNPROVEN];"))
        (problem,) = checker.problems(root)
        self.assertIn("UNPROVEN needs one literal proven_at", problem)

    def test_a_symbolic_claim_cannot_admit_a_client(self):
        root = self.tree(151, [151], [151])
        path = root / "src/terminal/ipc/older_clients.rs"
        path.write_text(path.read_text().replace("proven_at: 151,", "proven_at: PROTOCOL_VERSION,"))
        (problem,) = checker.problems(root)
        self.assertIn("C0 needs one literal proven_at", problem)

    def test_a_bump_without_a_recording_fails(self):
        (problem,) = checker.problems(self.tree(152, [152], [128, 151]))
        self.assertIn("152.json is missing", problem)

    def test_a_client_proven_at_an_earlier_protocol_fails(self):
        (problem,) = checker.problems(self.tree(151, [129], [128, 129, 151]))
        self.assertIn("proven at protocol 129, not 151", problem)

    def test_a_recording_newer_than_the_protocol_fails(self):
        (problem,) = checker.problems(self.tree(151, [151], [151, 152]))
        self.assertIn("152.json is newer", problem)

    def test_a_moved_older_client_list_fails(self):
        root = self.tree(151, [151], [151])
        (root / "src/terminal/ipc/older_clients.rs").unlink()
        (problem,) = checker.problems(root)
        self.assertIn("older_clients.rs not found", problem)


if __name__ == "__main__":
    unittest.main()
