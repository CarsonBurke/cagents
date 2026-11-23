"""Contracts for scripts/check-credential-guard."""

import importlib.machinery
import importlib.util
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
SCRIPT = ROOT / "scripts" / "check-credential-guard"
loader = importlib.machinery.SourceFileLoader("check_credential_guard", str(SCRIPT))
spec = importlib.util.spec_from_loader(loader.name, loader)
guard = importlib.util.module_from_spec(spec)
loader.exec_module(guard)

# Needles assembled here too, so this suite does not name them literally.
SIGN_IN = "".join(("auth", ".json"))
REFRESH = "".join(("refresh", "Token"))
SERVICE = "".join(("api.open", "ai.com"))

ACCOUNTS = '''pub fn credential_overrides(provider: Provider) -> &'static [&'static str] {
    match provider {
        Provider::Claude => &["ANTHROPIC_API_KEY", "CLAUDE_CODE_OAUTH_TOKEN"],
        _ => &[],
    }
}
'''


class Repository(unittest.TestCase):
    def tree(self, files):
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        root = Path(directory.name)
        (root / "src").mkdir()
        (root / "src/account.rs").write_text(ACCOUNTS)
        # Enough ordinary sources for the scan to mean something.
        for index in range(60):
            (root / "src" / f"plain_{index}.rs").write_text("fn plain() {}\n")
        for name, text in files.items():
            (root / name).parent.mkdir(parents=True, exist_ok=True)
            (root / name).write_text(text)
        return root

    def test_this_repository_passes(self):
        self.assertEqual(guard.problems(ROOT), [])

    def test_a_service_host_or_credential_field_in_code_is_named(self):
        root = self.tree({
            "src/client.rs": f'const HOST: &str = "https://{SERVICE}";\n',
            "scripts/helper": f"jq .{REFRESH} \"$HOME/.codex/{SIGN_IN}\"\n",
            "tests/fixtures/flow.sh": f"curl https://{SERVICE}\n",
        })
        found = guard.problems(root)
        self.assertIn(f"src/client.rs names {SERVICE[4:]}", found)
        self.assertIn(f"scripts/helper names {REFRESH}", found)
        self.assertIn(f"scripts/helper names {SIGN_IN}", found)
        self.assertIn(f"tests/fixtures/flow.sh names {SERVICE[4:]}", found)

    def test_recorded_data_and_the_reset_transport_are_outside_the_scan(self):
        root = self.tree({
            "tests/fixtures/recorded.jsonl": f'{{"url": "https://{SERVICE}"}}\n',
            "scripts/fixture.py": f"URL = 'https://{SERVICE}'\n",
            guard.RESET_TRANSPORT: f'const HOST: &str = "https://{SERVICE}";\n',
        })
        self.assertEqual(guard.problems(root), [])

    def test_only_the_exact_quoted_credential_variable_names_are_permitted(self):
        root = self.tree({
            "src/launch.rs": 'env.remove("CLAUDE_CODE_OAUTH_TOKEN");\n',
            "src/leak.rs": 'let token = env::var("CLAUDE_CODE_OAUTH_TOKEN_VALUE");\n',
        })
        self.assertEqual(guard.problems(root), ["src/leak.rs names OAUTH_TOKEN"])

    def test_a_tree_with_too_few_sources_is_refused(self):
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        root = Path(directory.name)
        (root / "src").mkdir()
        (root / "src/account.rs").write_text(ACCOUNTS)
        self.assertIn("too few", guard.problems(root)[0])


class Exceptions(unittest.TestCase):
    def test_muse_sign_in_exception_allows_only_its_named_mentions(self):
        probe = Path("src/account/probe/inputs.rs")
        fixture = Path("tests/fixtures/fixture-acp.sh")
        evidence = Path("scripts/native-evidence")
        path = f'account.home_directory(environment)?.join("{SIGN_IN}")'
        join = f"Some({path})"
        record = f'"$fixture/config/muse/{SIGN_IN}"'
        self.assertNotIn(SIGN_IN, guard.without_muse_sign_in_record(probe, join))
        self.assertNotIn(SIGN_IN, guard.without_muse_sign_in_record(fixture, record))
        self.assertNotIn(SIGN_IN, guard.without_muse_sign_in_record(evidence, record))
        for file, source in [
            (probe, f'std::fs::read("{SIGN_IN}")'),
            (probe, f"Some(std::fs::read({path}))"),
            (probe, f'home.join("{SIGN_IN}")'),
            (probe, record),
            (fixture, f'"$HOME/.codex/{SIGN_IN}"'),
            (fixture, join),
            (Path("src/account.rs"), join),
            (Path("scripts/other"), record),
        ]:
            with self.subTest(file=file, source=source):
                self.assertIn(SIGN_IN, guard.without_muse_sign_in_record(file, source))

    def test_native_refresh_exception_allows_only_the_boolean_request_expression(self):
        adapter = Path("src/chat/protocol/codex.rs")
        request = f'json!({{"{REFRESH}":true}})'
        self.assertNotIn(REFRESH, guard.without_native_refresh_request(adapter, request))
        for source in [
            f'result["{REFRESH}"].as_str()',
            f'json!({{"{REFRESH}":"credential-value"}})',
            f'json!({{"{REFRESH}":true_credentials}})',
        ]:
            with self.subTest(source=source):
                self.assertIn(REFRESH, guard.without_native_refresh_request(adapter, source))
        self.assertIn(REFRESH, guard.without_native_refresh_request(Path("src/account.rs"), request))


if __name__ == "__main__":
    unittest.main()
