#!/usr/bin/env python3
"""Private source/ledger fixtures for the selected IPC contract guard."""

import importlib.machinery
import importlib.util
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[1]
CHECKER = ROOT / "scripts/check-protocol-contract"


def load(name, path):
    loader = importlib.machinery.SourceFileLoader(name, str(path))
    spec = importlib.util.spec_from_loader(name, loader)
    module = importlib.util.module_from_spec(spec)
    loader.exec_module(module)
    return module


SOURCE = load("protocol_contract_source", ROOT / "scripts/protocol_contract_source.py")
CHECK = load("check_protocol_contract", CHECKER)
CHECK.SOURCE = SOURCE


class SourceContractTests(unittest.TestCase):
    def test_lexical_noise_and_nonserialization_derives_do_not_change_shape(self):
        first = '#[derive(Debug, Serialize, Deserialize)] pub struct Item { pub value: u8 }'
        second = '''/* nested /* comment */ still a comment */
            /// Documenting { Item } is not a field.
            #[allow(dead_code)]
            #[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
            pub struct Item { pub value : u8 }
            impl Item { fn runtime_only(&self) { let _ = "{ }"; } }
            #[cfg(test)] mod tests { fn unrelated() {} }
        '''
        self.assertEqual(SOURCE.selected_items(first, ("Item",)), SOURCE.selected_items(second, ("Item",)))

    def test_raw_strings_nested_comments_chars_and_lifetimes_keep_boundaries(self):
        text = '''#[serde(rename = r###"a } /* not a comment */"###)]
            pub struct Borrowed<'a, T> where T: Copy {
                #[serde(default = "with_escaped_\\\"_quote")]
                value: &'a T,
                bytes: [u8; b'}' as usize],
            }
        '''
        item = SOURCE.selected_items(text, ("Borrowed",))["Borrowed"]
        self.assertIn('r###"a } /* not a comment */"###', item)
        self.assertIn("'a", item)
        self.assertIn("b'}'", item)
        self.assertIn("where", item)

    def test_order_types_attributes_discriminants_and_tuple_headers_are_covered(self):
        baseline = SOURCE.selected_items('#[derive(Serialize)] pub enum Item { A(u8), B = 4 }', ("Item",))
        for text in (
            '#[derive(Serialize)] pub enum Item { B = 4, A(u8) }',
            '#[derive(Serialize)] pub enum Item { A(u16), B = 4 }',
            '#[derive(Serialize)] pub enum Item { A(u8), B = 5 }',
            '#[derive(Serialize)] #[serde(tag="kind")] pub enum Item { A(u8), B = 4 }',
            '#[derive(Serialize)] pub struct Item(u8);',
        ):
            with self.subTest(text=text):
                self.assertNotEqual(baseline, SOURCE.selected_items(text, ("Item",)))

    def test_missing_duplicate_and_malformed_selected_sources_refuse(self):
        for text in (
            'pub struct Other;',
            'pub struct Item; pub struct Item;',
            'pub struct Item { value: u8',
            'pub struct Item; /* unterminated',
            'pub struct Item { value: [u8; 2) }',
        ):
            with self.subTest(text=text), self.assertRaises(ValueError):
                SOURCE.selected_items(text, ("Item",))

    def test_version_requires_one_literal_u16_declaration(self):
        self.assertEqual(SOURCE.protocol_version('pub const PROTOCOL_VERSION: u16 = 127;'), 127)
        for text in (
            'pub const PROTOCOL_VERSION: u16 = 127 + 1;',
            'pub const PROTOCOL_VERSION: u16 = 70000;',
            'pub const PROTOCOL_VERSION: u16 = 127; pub const PROTOCOL_VERSION: u16 = 128;',
            '// pub const PROTOCOL_VERSION: u16 = 127;',
        ):
            with self.subTest(text=text), self.assertRaises(ValueError):
                SOURCE.protocol_version(text)


class LedgerContractTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="cagents-protocol-contract-")
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        (self.root / "src/terminal").mkdir(parents=True)
        (self.root / "tests/fixtures").mkdir(parents=True)
        self.write("src/terminal/ipc.rs", '''pub const PROTOCOL_VERSION: u16 = 127;
            #[derive(Serialize, Deserialize)] pub enum ClientRequest { Connect, List }
            #[derive(Serialize, Deserialize)] pub enum ServerMessage { Response(ServerResponse), Event }
            #[derive(Serialize, Deserialize)] pub enum ServerResponse { Accepted, Listed { count: u8 } }
        ''')
        self.write("src/agent.rs", '#[derive(Serialize, Deserialize)] pub enum Provider { Omp, Claude, Codex }')
        self.write("src/agent_execution.rs", '''
            #[derive(Serialize, Deserialize)] pub struct Run { pub id: u64, pub state: u8 }
            #[derive(Serialize, Deserialize)] pub struct Summary { pub agent: u64, pub state: u8 }
            #[derive(Serialize, Deserialize)] pub enum Reply { Status { agent: u64, run: Run }, Listed(Vec<Summary>) }
        ''')
        self.ok("--record")

    def write(self, path, text):
        (self.root / path).write_text(text)

    def replace(self, path, old, new):
        file = self.root / path
        self.assertIn(old, file.read_text())
        file.write_text(file.read_text().replace(old, new, 1))

    def run_guard(self, *args):
        return subprocess.run([sys.executable, "-I", "-B", str(CHECKER), "--root", str(self.root), *args], capture_output=True, text=True)

    def ok(self, *args):
        result = self.run_guard(*args)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        return result

    def refuses(self, *args, contains=None):
        result = self.run_guard(*args)
        self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
        if contains:
            self.assertIn(contains, result.stdout + result.stderr)
        return result

    def git(self, *args, success=True):
        result = subprocess.run(["git", "-C", str(self.root), *args], capture_output=True, text=True)
        if success:
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        return result

    def commit(self):
        self.git("add", ".")
        self.git("-c", "user.name=Contract fixture", "-c", "user.email=fixture@example.invalid", "commit", "-qm", "Private contract fixture")
        return self.git("rev-parse", "HEAD").stdout.strip()

    def base(self):
        self.git("init", "-q", "--initial-branch=main")
        return self.commit()

    def bump(self, value=128):
        self.replace("src/terminal/ipc.rs", "PROTOCOL_VERSION: u16 = 127", f"PROTOCOL_VERSION: u16 = {value}")

    def test_current_repository_selected_contract_matches(self):
        result = subprocess.run([sys.executable, "-I", "-B", str(CHECKER)], capture_output=True, text=True)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("7 selected declarations", result.stdout)

    def test_reported_collisions_each_refuse_without_a_new_number(self):
        for path, before, after in (
            ("src/terminal/ipc.rs", "{ Connect, List }", "{ Connect, List, StartupState }"),
            ("src/terminal/ipc.rs", "Accepted, Listed", "Accepted, StartupState { count: u64 }, Listed"),
            ("src/agent.rs", "Omp, Claude, Codex", "Omp, Claude, Codex, Antigravity"),
            ("src/agent_execution.rs", "pub id: u64,", "pub id: u64, pub queued_ms: u64,"),
            ("src/agent_execution.rs", "pub agent: u64,", "pub agent: u64, pub provider: Provider,"),
            ("src/agent_execution.rs", "Status { agent: u64,", "Status { provider: Provider, agent: u64,"),
        ):
            with self.subTest(after=after):
                source = (self.root / path).read_text()
                self.replace(path, before, after)
                result = self.refuses(contains="already has a different selected source contract")
                self.assertIn(path + "::", result.stderr)
                self.assertIn("scripts/check-protocol-contract --record", result.stderr)
                self.assertIn("custom Serde", result.stderr)
                self.refuses("--record", contains="unused higher PROTOCOL_VERSION")
                self.write(path, source)

    def test_nonshape_edits_leave_the_snapshot_valid(self):
        with (self.root / "src/agent.rs").open("a") as output:
            output.write('\n// Runtime change\nimpl Provider { fn label(&self) -> &str { "new label" } }')
        self.replace("src/agent.rs", "#[derive(Serialize, Deserialize)]", "#[derive(Debug, Clone, Serialize, Deserialize)]")
        self.ok()

    def test_new_higher_version_adds_a_snapshot_and_keeps_the_previous_one(self):
        base = self.base()
        before = (self.root / CHECK.FIXTURES / "127.json").read_bytes()
        self.replace("src/agent.rs", "Codex }", "Codex, Antigravity }")
        self.bump()
        self.refuses(contains="Missing protocol 128 snapshot")
        self.ok("--record", "--base-ref", base)
        self.ok("--base-ref", base)
        self.assertEqual((self.root / CHECK.FIXTURES / "127.json").read_bytes(), before)
        self.assertTrue((self.root / CHECK.FIXTURES / "128.json").is_file())

    def test_manually_rewriting_the_same_number_cannot_bless_a_shape_change(self):
        base = self.base()
        self.replace("src/agent.rs", "Codex }", "Codex, Antigravity }")
        changed = CHECK.contract(lambda path: (self.root / path).read_text())
        self.write(f"{CHECK.FIXTURES}/127.json", CHECK.encoded(changed))
        self.ok()  # A trusted comparison is what makes the old record immutable.
        self.refuses("--base-ref", base, contains="snapshots are immutable")

    def test_deleting_an_old_snapshot_is_refused_by_the_base(self):
        base = self.base()
        self.bump()
        self.ok("--record")
        (self.root / CHECK.FIXTURES / "127.json").unlink()
        self.refuses("--base-ref", base, contains="snapshots are immutable")

    def test_unreadable_comparison_base_fails_explicitly(self):
        self.base()
        self.refuses("--base-ref", "f" * 40, contains="Cannot read trusted comparison base")

    def test_present_guard_without_its_current_snapshot_is_refused(self):
        (self.root / CHECK.FIXTURES / "127.json").unlink()
        self.refuses(contains="Missing protocol 127 snapshot")

    def test_first_adoption_still_compares_selected_base_source(self):
        (self.root / CHECK.FIXTURES / "127.json").unlink()
        base = self.base()
        self.replace("src/agent.rs", "Codex }", "Codex, Antigravity }")
        self.ok("--record")
        self.refuses("--base-ref", base, contains="without a new protocol number")

    def test_parallel_different_contracts_at_the_same_new_number_conflict(self):
        base = self.base()
        self.git("checkout", "-qb", "left")
        self.bump()
        self.replace("src/agent.rs", "Codex }", "Codex, Left }")
        self.ok("--record", "--base-ref", base)
        left = self.commit()
        self.git("checkout", "-qb", "right", base)
        self.bump()
        self.replace("src/agent_execution.rs", "pub id: u64,", "pub id: u64, pub right: u64,")
        self.ok("--record", "--base-ref", base)
        self.commit()
        result = self.git("-c", "user.name=Contract fixture", "-c", "user.email=fixture@example.invalid", "merge", "--no-commit", left, success=False)
        self.assertNotEqual(result.returncode, 0)
        path = f"{CHECK.FIXTURES}/128.json"
        self.assertIn(f"CONFLICT (add/add): Merge conflict in {path}", result.stdout)
        unmerged = self.git("ls-files", "-u", "--", path).stdout.splitlines()
        self.assertEqual(len(unmerged), 2, unmerged)

    def test_selected_source_and_snapshot_symlinks_are_refused(self):
        path = self.root / "src/agent.rs"
        path.rename(path.with_suffix(".real"))
        path.symlink_to(path.with_suffix(".real"))
        self.refuses(contains="never a symlink")
        path.unlink()
        path.with_suffix(".real").rename(path)
        snapshot = self.root / CHECK.FIXTURES / "127.json"
        real_snapshot = self.root / "tests/fixtures/snapshot-real.json"
        snapshot.rename(real_snapshot)
        snapshot.symlink_to(real_snapshot)
        self.refuses(contains="never a symlink")

    def test_malformed_or_wrong_version_snapshots_refuse(self):
        path = self.root / CHECK.FIXTURES / "127.json"
        saved = json.loads(path.read_text())
        for invalid in ({}, {**saved, "format": True}, {**saved, "protocol": 128}, {**saved, "items": {}}, {**saved, "items": {"Item": [False]}}):
            with self.subTest(invalid=invalid):
                path.write_text(json.dumps(invalid))
                self.refuses(contains="Malformed protocol snapshot")
        path.write_text("{")
        self.refuses()


if __name__ == "__main__":
    unittest.main()
