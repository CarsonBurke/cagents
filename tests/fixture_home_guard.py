#!/usr/bin/python3
"""Offline fixture-home entry contracts; no native application is launched.

Every executable fixture is classified below. Provider stand-ins and seeders
are exercised with invented homes under /var/tmp. Scene acceptance is limited
to startup: a Bash DEBUG trap stops immediately after the real shared guard
returns, before any native-evidence command or scene action can run.
"""

from __future__ import annotations

from dataclasses import dataclass
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import sqlite3
import subprocess
import sys
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[1]
FIXTURES = ROOT / "tests/fixtures"
MARKER = ".cagents-fixture-root"
SHELL_HELPER = FIXTURES / "lib/fixture-home.sh"
PYTHON_HELPER = ROOT / "scripts/fixture_home.py"
JS_HELPER = FIXTURES / "lib/fixture-home.js"


@dataclass(frozen=True)
class Case:
    filename: str
    variable: str
    mode: str = "stdio"
    installed: str | None = None


# This is deliberately an explicit inventory: a new executable cannot silently
# inherit an exemption or escape the dynamic refusal contracts.
CASES = (
    Case("automation-claude.sh", "CLAUDE_CONFIG_DIR"),
    Case("automation-establishment-claude.sh", "CLAUDE_CONFIG_DIR"),
    Case("chat-composer-claude.sh", "CLAUDE_CONFIG_DIR", "version"),
    Case("console-transcript-claude.sh", "CLAUDE_CONFIG_DIR"),
    Case("execution-console-claude.sh", "CLAUDE_CONFIG_DIR"),
    Case("wait-outcomes-claude.sh", "CLAUDE_CONFIG_DIR"),
    Case("fixture-codex.sh", "CODEX_HOME", "login", "codex"),
    Case("rehearsal-codex.sh", "CODEX_HOME", "version", "codex"),
    Case("fixture-acp.sh", "HOME", "acp", "agy"),
    Case("fixture-gh.sh", "HOME", "forge", "gh"),
    Case("fixture-xdg-open.sh", "HOME", "open", "xdg-open"),
    Case("linear-curl.sh", "HOME", "linear", "curl"),
    Case("claude-unknown-frames.py", "CLAUDE_CONFIG_DIR", "version"),
    Case("claude-upgrade-continuity.py", "CLAUDE_CONFIG_DIR", "continuity"),
    Case("rehearsal-claude-no-init.py", "CLAUDE_CONFIG_DIR", "version"),
    Case("rehearsal-claude-exit-mid-turn.py", "CLAUDE_CONFIG_DIR", "version"),
    Case("codex-child-errors.py", "CODEX_HOME", "version"),
    Case("codex-tool-rows.py", "CODEX_HOME", "version"),
    Case("codex-usage-resets.py", "CODEX_HOME", "version"),
    Case("rehearsal-codex.py", "CODEX_HOME", "version"),
    Case("terminal-escape-codex.py", "CODEX_HOME", "version"),
    Case("fake-acp-agent.py", "HOME"),
    Case("claude-long-history.py", "HOME", "long-history"),
    Case("claude-damaged-history.py", "HOME", "damaged-history"),
    Case("conversation-work.py", "HOME", "conversation"),
    Case("live-agent-status-seed.py", "HOME", "status-seed"),
    Case("seed-refusal-registry.py", "HOME", "registry"),
    Case("unnamed-agent-thread.py", "HOME", "unnamed"),
    Case("claude-resume-history.py", "CLAUDE_CONFIG_DIR", "root-claude"),
    Case("codex-resume-history.py", "CODEX_HOME", "root-codex"),
    Case("usage-reset-fixture.py", "CLAUDE_CONFIG_DIR", "root-reset"),
    Case("native-account-registry.py", "HOME", "root-account"),
    Case("native-retired-ownership.py", "HOME", "root-retired"),
)

SCENES = (
    "account-switch-busy-scene.sh", "interrupt-scene.sh", "live-agent-status-scene.sh",
    "message-card-scene.sh", "nested-agents-scene.sh", "reasoning-row-scene.sh",
    "seed-refusal-scene.sh", "sidebar-rail-scene.sh", "spawn-label-attention-scene.sh",
    "spawn-placement-scene.sh", "transcript-rhythm-scene.sh", "unnamed-agent-thread-scene.sh",
    "update-resume-scene.sh", "waiting-agent-scene.sh",
)

NOT_APPLICABLE = {
    "chat-footer-scene.sh": "all writes go through native-evidence env/cli",
    "declined-send-scene.sh": "all writes go through native-evidence env/cli",
    "delivery-notices-scene.sh": "all writes go through native-evidence env/cli",
    "pending-terminal-save-scene.sh": "all writes go through native-evidence env/cli",
    "usage-reset-scene.sh": "all writes go through native-evidence env/cli",
    "check-claude-resume-history.py": "unittest harness creates invented fixture roots",
    "test_seed_refusal_registry.py": "unittest harness creates invented fixture roots",
    "reasoning-row-scene/locate.py": "read-only OCR parser; no provider or app state",
    "reasoning-row-scene/test_locate.py": "read-only OCR parser unittest harness",
    "lib/fixture-home.sh": "shared guard definition, not an executable provider",
    "lib/fixture-home.js": "shared guard definition for future JavaScript fixtures",
}

# Its C compilation and real bind/log startup contracts live in this suite;
# it is still covered by the inventory and must call its language's guard.
EXTERNAL_GUARDED = {
    "rehearsal-startup-bind.c": "tests/rehearse_update_retry_bind.py",
}


def snapshot(root):
    """Notice creation, deletion, replacement, contents, mode and timestamps."""
    found = {}
    for path in (root, *sorted(root.rglob("*"))):
        info = path.lstat()
        contents = (os.readlink(path) if path.is_symlink() else
                    hashlib.sha256(path.read_bytes()).hexdigest() if path.is_file() else None)
        found[str(path.relative_to(root))] = (info.st_mode, info.st_mtime_ns,
                                             info.st_ctime_ns, info.st_ino, contents)
    return found


class Home(unittest.TestCase):
    def setUp(self):
        self.scratch = tempfile.TemporaryDirectory(prefix="fixture-home-contract-", dir="/var/tmp")
        self.addCleanup(self.scratch.cleanup)
        self.base = Path(self.scratch.name)
        self.task = self.base / "task"
        self.task.mkdir(mode=0o700)
        (self.task / ".native-evidence").touch(mode=0o600)
        self.home = self.task / "fixture"
        self.home.mkdir(mode=0o700)
        for name in ("bin", "home", "home/.claude", "home/.codex", "home/.config",
                     "runtime", "data", "config", "state", "logs", "project/.git", "acp/agy"):
            (self.home / name).mkdir(parents=True, exist_ok=True, mode=0o700)
        self.account = self.base / "account"
        self.account.mkdir(mode=0o700)
        (self.account / "credentials.json").write_text('{"invented":"keep exactly"}\n')
        (self.account / "sessions").mkdir(mode=0o700)
        (self.account / "sessions/keep.jsonl").write_text('{"invented":"history"}\n')
        self.env = {
            "PATH": os.environ.get("PATH", "/usr/bin:/bin"),
            "HOME": str(self.home / "home"),
            "CLAUDE_CONFIG_DIR": str(self.home / "home/.claude"),
            "CODEX_HOME": str(self.home / "home/.codex"),
            "XDG_RUNTIME_DIR": str(self.home / "runtime"),
            "XDG_DATA_HOME": str(self.home / "data"),
            "XDG_CONFIG_HOME": str(self.home / "config"),
            "XDG_CACHE_HOME": str(self.home / "home/.cache"),
            "FAKE_ACP_STATE": str(self.home / "acp/agy"),
            "CLAUDE_ESTABLISHMENT_FIXTURE_ROOT": str(self.home),
            "LINEAR_FIXTURE": str(self.home),
            "CAGENTS_EVIDENCE_FIXTURE": str(self.home),
            "CAGENTS_EVIDENCE_TASK": str(self.task),
            "CAGENTS_TOOL_ROWS_FIXTURE_ROOT": str(self.home),
            "CAGENTS_CHILD_ERROR_FIXTURE_ROOT": str(self.home),
            "CAGENTS_USAGE_RESETS_FIXTURE_ROOT": str(self.home),
            "CAGENTS_ESCAPE_FIXTURE_ROOT": str(self.home),
            "TMPDIR": str(self.base),
            "PYTHONDONTWRITEBYTECODE": "1",
            "GIT_CONFIG_GLOBAL": "/dev/null",
            "GIT_CONFIG_NOSYSTEM": "1",
        }

    def marked(self):
        (self.home / MARKER).touch(mode=0o600)

    def run_command(self, command, *, env=None, cwd=None):
        return subprocess.run(command, input="", text=True, capture_output=True,
                              env=self.env if env is None else env,
                              cwd=self.home if cwd is None else cwd, timeout=15)

    def assert_refusal(self, result, variable, path):
        self.assertNotEqual(result.returncode, 0, result.stdout)
        lines = result.stderr.splitlines()
        self.assertEqual(len(lines), 1, result.stderr)
        self.assertIn("fixture", lines[0].lower())
        self.assertIn("refus", lines[0].lower())
        self.assertIn(variable, lines[0])
        self.assertIn(str(path), lines[0])

    def install(self, case):
        target = self.home / "bin" / case.installed
        shutil.copy2(FIXTURES / case.filename, target)
        # Installed fixtures retain their usual native-evidence directory shape.
        for leaf in ("bin/lib", "libexec", "libexec/codex", "libexec/acp", "scripts", "tests/fixtures/lib"):
            (self.home / leaf).mkdir(parents=True, exist_ok=True)
        for leaf in ("libexec/fixture-home.sh", "libexec/fixture_home.py",
                     "libexec/codex/fixture_home.py", "libexec/acp/fixture_home.py",
                     "scripts/fixture_home.py"):
            shutil.copy2(PYTHON_HELPER if leaf.endswith(".py") else SHELL_HELPER,
                         self.home / leaf)
        shutil.copy2(SHELL_HELPER, self.home / "tests/fixtures/lib/fixture-home.sh")
        shutil.copy2(SHELL_HELPER, self.home / "bin/lib/fixture-home.sh")
        for source in FIXTURES.glob("*.py"):
            shutil.copy2(source, self.home / "libexec/codex" / source.name)
        shutil.copy2(FIXTURES / "fake-acp-agent.py", self.home / "libexec/acp/fake-acp-agent.py")
        (self.home / "libexec/codex/peer").write_text("tool-rows\n")
        (self.home / "home/.config/fixture-forge").touch()
        (self.home / "read").write_text('{"data":{}}\n')
        return target

    def prepare_case(self, case):
        script = self.install(case) if case.installed else FIXTURES / case.filename
        command = (["sh", str(script)] if script.suffix == ".sh" or case.installed
                   else [sys.executable, "-I", "-B", str(script)])
        mode = case.mode
        if mode == "version":
            command.append("--version")
        elif mode == "login":
            command.extend(("login", "status"))
        elif mode == "acp":
            command.append("acp")
        elif mode == "forge":
            command.extend(("auth", "status", "--json", "hosts"))
        elif mode == "open":
            command.append("https://example.invalid/fixture")
        elif mode == "long-history":
            command.extend((str(self.home / "project"), "--turns", "129"))
        elif mode == "damaged-history":
            command.append(str(self.home / "project"))
        elif mode == "status-seed":
            command.append(str(self.home))
        elif mode == "root-claude" or mode == "root-codex":
            # These seeders deliberately require a fresh provider directory.
            shutil.rmtree(self.home / ("home/.claude" if mode == "root-claude" else "home/.codex"))
            command.extend((str(self.home), "--turns", "1"))
        elif mode == "root-reset":
            command.extend(("seed", str(self.home)))
        elif mode == "root-account":
            command.extend(("prepare", str(self.home)))
        elif mode == "root-retired":
            command.extend((str(self.home), "11111111-2222-4333-8444-555555555555"))
        elif mode == "registry" or mode == "unnamed":
            directory = self.home / "data/cagents-development"
            directory.mkdir()
            with sqlite3.connect(directory / "workspace-model.sqlite3") as database:
                database.execute("CREATE TABLE agents(id TEXT,record TEXT,closed INTEGER)")
                database.execute("CREATE TABLE agent_authorities(token TEXT,source TEXT,session_id TEXT)")
                database.execute("CREATE TABLE panes(id TEXT,kind TEXT,resource TEXT,descriptor TEXT,provider_session TEXT)")
                if mode == "unnamed":
                    record = {"session": {"Chat": {"id": "invented-chat",
                              "launch": {"provider": "Codex", "session_id": "invented-thread"}}}}
                    database.execute("INSERT INTO agents VALUES(?,?,0)", ("invented-agent", json.dumps(record)))
            command.append("status" if mode == "registry" else "forget")
        elif mode == "conversation":
            (self.home / "project/.git").rmdir()
            for arguments in (("init", "--quiet", "--initial-branch=main"),
                              ("-c", "user.name=Fixture", "-c", "user.email=fixture@example.invalid",
                               "commit", "--quiet", "--allow-empty", "--message", "Fixture project")):
                result = self.run_command(["git", *arguments], cwd=self.home / "project")
                self.assertEqual(result.returncode, 0, result.stderr)
        elif mode == "continuity":
            installed = self.base / "updater1513-contract"
            installed.mkdir(mode=0o700)
            (installed / MARKER).touch(mode=0o600)
            (installed / "bin").mkdir()
            (installed / "scripts").mkdir()
            shutil.copy2(PYTHON_HELPER, installed / "scripts/fixture_home.py")
            target = installed / "bin/claude"
            shutil.copy2(script, target)
            command = [sys.executable, "-I", "-B", str(target), "--version"]
        return command


def fixture_contract(case, kind):
    def contract(self):
        command = self.prepare_case(case)
        explicit = case.mode.startswith("root-")
        if kind == "marked":
            self.marked()
        elif explicit:
            self.env.pop(case.variable, None)
        elif kind == "unmarked":
            # Other guards receive a valid fixture; the selected effective
            # config path alone points at an invented live-looking account.
            self.marked()
            self.env[case.variable] = str(self.account)
        else:
            self.env.pop(case.variable, None)
        before = snapshot(self.base)
        if kind == "marked" and case.mode == "root-retired":
            # Its normal mutation requires a retired native actor. Acceptance
            # here stops after the real guard, without opening a model. Its
            # model mutation is covered by tests/native_retired_ownership.py.
            wrapper = """import runpy,sys
target=sys.argv.pop(1)
def trace(frame,event,arg):
    if event=='return' and frame.f_code.co_name=='require_fixture_home':
        if arg is not None:
            print('fixture startup accepted')
            raise SystemExit(0)
    return trace
sys.settrace(trace)
runpy.run_path(target,run_name='__main__')
"""
            command = [sys.executable, "-I", "-B", "-c", wrapper, *command[3:]]
        result = self.run_command(command)
        if kind == "marked":
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertNotIn("fixture-home: refusing", result.stderr)
        else:
            path = (self.home / "home" if case.variable == "HOME" else
                    self.home / "home" / (".codex" if case.variable == "CODEX_HOME" else ".claude"))
            expected = path.resolve() if explicit else self.account if kind == "unmarked" else "<unset>"
            # Installed Codex launchers supply their own fixture-home default;
            # it is refused when that installation has no positive marker.
            if kind == "unset" and case.installed == "codex":
                expected = (self.home / "home/.codex").resolve()
            self.assert_refusal(result, case.variable, expected)
            self.assertEqual(snapshot(self.base), before, "refusal changed fixture/account files")
    return contract


class FixtureEntryContracts(Home):
    pass


for _case in CASES:
    for _kind in ("unmarked", "unset", "marked"):
        setattr(FixtureEntryContracts,
                "test_" + _case.filename.replace(".", "_").replace("-", "_") + "_" + _kind,
                fixture_contract(_case, _kind))


class SceneEntryContracts(Home):
    def scene(self, filename, marked, unset=False, *, refused_variable=None, refused_path=None):
        if marked:
            self.marked()
        if unset:
            self.env.pop("CAGENTS_EVIDENCE_TASK", None)
            self.env.pop("HOME", None)
        script = FIXTURES / filename
        # Tracing applies only to the real scene, not to sourced helper lines
        # or helper function bodies. The next scene line proves the real guard
        # returned successfully. No native-evidence invocation is permitted.
        callbacks = filename in ("spawn-label-attention-scene.sh", "spawn-placement-scene.sh", "update-resume-scene.sh")
        # These three scenes are sourced by native-evidence callbacks. Supply
        # only the read-only fixture-path response they need before the guard.
        runner = r'''set -T
fixture_contract_script=$1
shift
evidence() {
  if [[ $* == 'env sh -c printf %s "$CAGENTS_EVIDENCE_FIXTURE"' ]]; then
    printf '%s' "$CAGENTS_EVIDENCE_FIXTURE"
  else
    printf '%s\n' 'native commands are forbidden in startup-only contracts' >&2
    exit 99
  fi
}
fixture_contract_guard_seen=
trap 'if [[ ${BASH_SOURCE[0]:-} == "$fixture_contract_script" ]]; then
  case $BASH_COMMAND in
    require_fixture_home\ *) fixture_contract_guard_seen=1 ;;
    *) if [[ -n $fixture_contract_guard_seen ]]; then
         printf "%s\n" "fixture scene startup accepted"
         exit 0
       fi ;;
  esac
fi' DEBUG
source "$fixture_contract_script" "$@"
'''
        before = snapshot(self.base)
        result = self.run_command(["bash", "-c", runner, "fixture-scene-contract", str(script), "wide"])
        if marked and refused_variable is None:
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(result.stdout.strip(), "fixture scene startup accepted")
        else:
            expected = (refused_path if refused_variable else
                        "<unset>" if unset and not callbacks else (self.home / "home").resolve())
            self.assert_refusal(result, refused_variable or "HOME", expected)
        self.assertEqual(snapshot(self.base), before, "startup contract reached a scene write")

    def test_marked_scene_refuses_symlinked_provider_config_before_native_commands(self):
        self.marked()
        config = self.home / "home/.claude"
        config.rmdir()
        config.symlink_to(self.account, target_is_directory=True)
        self.scene("nested-agents-scene.sh", True,
                   refused_variable="CLAUDE_CONFIG_DIR", refused_path=self.account)


for _scene in SCENES:
    for _kind in ("unmarked", "unset", "marked"):
        def _scene_contract(self, scene=_scene, kind=_kind):
            self.scene(scene, kind == "marked", kind == "unset")
        setattr(SceneEntryContracts, "test_" + _scene.replace(".", "_").replace("-", "_") + "_" + _kind,
                _scene_contract)


class Inventory(unittest.TestCase):
    def test_every_script_is_enumerated_and_provider_like_scripts_call_shared_guard(self):
        paths = {str(path.relative_to(FIXTURES)) for path in FIXTURES.rglob("*")
                 if path.is_file() and path.suffix in (".sh", ".py", ".js", ".c")}
        guarded = {case.filename for case in CASES} | set(SCENES) | set(EXTERNAL_GUARDED)
        self.assertEqual(paths, guarded | set(NOT_APPLICABLE),
                         "classify each new fixture and add its dynamic guard contracts")
        self.assertFalse(guarded & set(NOT_APPLICABLE))
        for filename in sorted(guarded):
            with self.subTest(fixture=filename):
                source = (FIXTURES / filename).read_text()
                self.assertRegex(source, r"\brequire_fixture_home\s*\(?",
                                 "provider/app-state fixture must call the shared helper")
                helper = "fixture-home.sh" if filename.endswith(".sh") else "fixture_home"
                self.assertIn(helper, source)
        # Reference scanning also catches a provider fixture whose name does
        # not identify it. Exemptions above each explain why no guard applies.
        provider_reference = re.compile(r"CLAUDE_CONFIG_DIR|CODEX_HOME|\bHOME\b|\.claude|\.codex")
        for filename in sorted(paths - guarded):
            with self.subTest(unprotected=filename):
                if provider_reference.search((FIXTURES / filename).read_text()):
                    self.assertTrue(NOT_APPLICABLE[filename])

    def test_workspace_state_boundaries_call_the_python_helper(self):
        guarded = ("apply.py", "claude.py", "codex.py", "codex_check.py", "repositories.py", "generate.py")
        not_applicable = {
            "__init__.py": "package marker",
            "cli.py": "dispatches to guarded generators and state writers",
            "content.py": "invented strings and in-memory content",
            "determinism.py": "in-memory deterministic random streams and formatting",
            "profiles.py": "in-memory workload definitions",
            "workspace.py": "in-memory plan construction",
        }
        paths = {path.name for path in (ROOT / "scripts/fixture_workspace").glob("*.py")}
        self.assertEqual(paths, set(guarded) | set(not_applicable),
                         "classify new fixture_workspace modules and guard state writers")
        for filename in guarded:
            with self.subTest(module=filename):
                source = (ROOT / "scripts/fixture_workspace" / filename).read_text()
                self.assertRegex(source, r"from fixture_home import .*require_fixture_home")
                self.assertRegex(source, r"require_fixture_home\(")
        self.assertIn("FIXTURE_ROOT_MARKER", (ROOT / "scripts/fixture_workspace/generate.py").read_text())


class SharedHelperContracts(Home):
    def command(self, language, variable="CLAUDE_CONFIG_DIR", explicit=None):
        if language == "sh":
            script = '. "$1"; shift; require_fixture_home "$@"; printf "%s\\n" "$fixture_home_resolved"'
            return ["sh", "-c", script, "fixture-helper", str(SHELL_HELPER), variable,
                    *([] if explicit is None else [str(explicit)])]
        if language == "python":
            script = ("import sys;sys.path.insert(0,sys.argv[1]);"
                      "from fixture_home import require_fixture_home;"
                      "print(require_fixture_home(sys.argv[2],*sys.argv[3:]))")
            return [sys.executable, "-I", "-B", "-c", script, str(PYTHON_HELPER.parent), variable,
                    *([] if explicit is None else [str(explicit)])]
        node = shutil.which("node")
        if node is None:
            self.skipTest("node is unavailable; JavaScript helper needs Node.js")
        script = ("const helper=require(process.argv[1]);"
                  "const requireHome=helper.requireFixtureHome || helper.require_fixture_home;"
                  "console.log(requireHome(process.argv[2], process.argv[3]));")
        return [node, "-e", script, str(JS_HELPER), variable,
                *([] if explicit is None else [str(explicit)])]

    def helper(self, language, scenario):
        self.marked()
        config = self.home / "home/.claude"
        expected = config.resolve()
        explicit = None
        accepted = scenario in ("marked-ancestor", "marker-at-config", "symlink-in", "nonexistent-child", "explicit")
        if scenario == "unmarked":
            self.env["CLAUDE_CONFIG_DIR"] = str(self.account)
            expected = self.account
        elif scenario == "unset":
            self.env.pop("CLAUDE_CONFIG_DIR")
            expected = "<unset>"
        elif scenario == "empty":
            self.env["CLAUDE_CONFIG_DIR"] = ""
            expected = "<unset>"
        elif scenario == "marker-at-config":
            (self.home / MARKER).unlink()
            (config / MARKER).touch()
        elif scenario == "marker-directory":
            (self.home / MARKER).unlink()
            (self.home / MARKER).mkdir()
            accepted = False
        elif scenario == "symlink-out":
            alias = self.home / "home/linked-account"
            alias.symlink_to(self.account, target_is_directory=True)
            self.env["CLAUDE_CONFIG_DIR"] = str(alias)
            expected = self.account
        elif scenario == "symlink-in":
            alias = self.account / "fixture-alias"
            alias.symlink_to(config, target_is_directory=True)
            self.env["CLAUDE_CONFIG_DIR"] = str(alias)
        elif scenario == "nonexistent-child":
            expected = config / "not-created/provider-home"
            self.env["CLAUDE_CONFIG_DIR"] = str(expected)
        elif scenario == "explicit":
            self.env.pop("CLAUDE_CONFIG_DIR")
            explicit = config
        elif scenario == "newline":
            expected = str(self.account / "line\nbreak").replace("\n", "\\n")
            self.env["CLAUDE_CONFIG_DIR"] = str(self.account / "line\nbreak")
        elif scenario == "newline-parent":
            # Command substitution must not trim a dirname's trailing newline
            # and accidentally inspect its marked, differently named sibling.
            unmarked = self.home.with_name(self.home.name + "\n")
            unmarked.mkdir(mode=0o700)
            config = unmarked / "provider-config"
            self.env["CLAUDE_CONFIG_DIR"] = str(config)
            expected = str(config).replace("\n", "\\n")
        elif scenario == "trailing-newline":
            # Reading a variable through command substitution must preserve
            # its final newline instead of authorizing a marked sibling.
            (self.account / MARKER).touch(mode=0o600)
            config = self.account.with_name(self.account.name + "\n")
            config.mkdir(mode=0o700)
            self.env["CLAUDE_CONFIG_DIR"] = str(config)
            expected = str(config).replace("\n", "\\n")
        before = snapshot(self.base)
        result = self.run_command(self.command(language, explicit=explicit))
        if accepted:
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(result.stdout.strip(), str(expected))
        else:
            self.assert_refusal(result, "CLAUDE_CONFIG_DIR", expected)
        self.assertEqual(snapshot(self.base), before, "shared guard must never create or modify files")


for _language in ("sh", "python", "js"):
    for _scenario in ("unmarked", "unset", "empty", "marked-ancestor", "marker-at-config",
                      "marker-directory", "symlink-out", "symlink-in", "nonexistent-child", "explicit", "newline",
                      "newline-parent", "trailing-newline"):
        def _helper_contract(self, language=_language, scenario=_scenario):
            self.helper(language, scenario)
        setattr(SharedHelperContracts, "test_" + _language + "_" + _scenario.replace("-", "_"),
                _helper_contract)


class StateWriterContracts(Home):
    def test_native_seed_restore_refuses_both_unmarked_sources_and_destinations_before_changes(self):
        seed = self.task / "fixture-seed"
        (seed / "home").mkdir(parents=True)
        (seed / "home/seed.txt").write_text("invented seed\n")
        target = self.home / "home/seed.txt"
        target.write_text("keep the original\n")
        target.chmod(0o400)
        command = [sys.executable, "-I", "-B", str(ROOT / "scripts/native-evidence-seed.py"),
                   str(seed), str(self.home)]
        before = snapshot(self.base)
        self.assert_refusal(self.run_command(command), "HOME", seed)
        self.assertEqual(snapshot(self.base), before)
        (seed / MARKER).touch(mode=0o600)
        before = snapshot(self.base)
        self.assert_refusal(self.run_command(command), "HOME", self.home)
        self.assertEqual(snapshot(self.base), before)
        self.marked()
        accepted = self.run_command(command)
        self.assertEqual(accepted.returncode, 0, accepted.stderr)
        self.assertEqual(target.read_text(), "invented seed\n")

    def test_native_seed_home_only_markers_do_not_authorize_sibling_state(self):
        seed = self.task / "fixture-seed"
        (seed / "home").mkdir(parents=True)
        for directory in (seed, self.home):
            (directory / "home" / MARKER).touch(mode=0o600)
            (directory / "data").mkdir(exist_ok=True)
            (directory / "data/keep").write_text("invented sibling state\n")
        before = snapshot(self.base)
        result = self.run_command([sys.executable, "-I", "-B",
                                  str(ROOT / "scripts/native-evidence-seed.py"), str(seed), str(self.home)])
        self.assert_refusal(result, "HOME", seed)
        self.assertEqual(snapshot(self.base), before)

    def workspace_writer(self, method, marked):
        config = self.home / "home/.codex" if marked else self.account
        if marked:
            self.marked()
        script = ("import sys;sys.path.insert(0,sys.argv[1]);"
                  "from fixture_workspace import codex;"
                  "home=sys.argv[2];"
                  + ("codex.write_index(home,[],1)" if method == "write_index" else
                     "[file.close() for file in codex.hold_writer_locks(home,['invented-thread'])]"))
        command = [sys.executable, "-I", "-B", "-c", script, str(ROOT / "scripts"), str(config)]
        before = snapshot(self.base)
        result = self.run_command(command)
        if marked:
            self.assertEqual(result.returncode, 0, result.stderr)
            filename = "session_index.jsonl" if method == "write_index" else "thread-writer-locks/invented-thread.lock"
            self.assertTrue((config / filename).is_file())
        else:
            self.assert_refusal(result, "CODEX_HOME", config)
            self.assertEqual(snapshot(self.base), before)

    def test_workspace_index_refuses_unmarked_home_without_changes(self):
        self.workspace_writer("write_index", False)

    def test_workspace_index_accepts_marked_home(self):
        self.workspace_writer("write_index", True)

    def test_workspace_writer_locks_refuse_unmarked_home_without_changes(self):
        self.workspace_writer("hold_writer_locks", False)

    def test_workspace_writer_locks_accept_marked_home(self):
        self.workspace_writer("hold_writer_locks", True)


SECONDARY_CASES = (
    (Case("fake-acp-agent.py", "HOME"), "FAKE_ACP_STATE", False),
    (Case("fixture-acp.sh", "HOME", "acp", "agy"), "FAKE_ACP_STATE", True),
    (Case("linear-curl.sh", "HOME", "linear", "curl"), "LINEAR_FIXTURE", False),
    (Case("automation-establishment-claude.sh", "CLAUDE_CONFIG_DIR"),
     "CLAUDE_ESTABLISHMENT_FIXTURE_ROOT", False),
    (Case("automation-claude.sh", "CLAUDE_CONFIG_DIR"), "HOME", False),
    (Case("execution-console-claude.sh", "CLAUDE_CONFIG_DIR"), "HOME", False),
)


class SecondaryPathContracts(Home):
    def secondary(self, case, variable, provided_default, scenario):
        command = self.prepare_case(case)
        self.marked()
        if scenario == "unmarked":
            self.env[variable] = str(self.account)
        elif scenario == "unset":
            self.env.pop(variable, None)
        before = snapshot(self.base)
        result = self.run_command(command)
        if scenario == "marked" or scenario == "unset" and provided_default:
            self.assertEqual(result.returncode, 0, result.stderr)
        else:
            self.assert_refusal(result, variable, self.account if scenario == "unmarked" else "<unset>")
            self.assertEqual(snapshot(self.base), before)

    def test_fake_acp_log_refuses_unmarked_output_before_writes(self):
        self.marked()
        self.env["FAKE_ACP_LOG"] = str(self.account / "provider.log")
        command = self.prepare_case(Case("fake-acp-agent.py", "HOME"))
        before = snapshot(self.base)
        self.assert_refusal(self.run_command(command), "FAKE_ACP_LOG", self.account / "provider.log")
        self.assertEqual(snapshot(self.base), before)

    def logging_parent(self, filename):
        self.marked()
        # HOME itself has a marker, while the ../spawns and ../prompts output
        # directory is outside it and must not be authorized by that marker.
        (self.account / MARKER).touch(mode=0o600)
        self.env["HOME"] = str(self.account)
        command = self.prepare_case(Case(filename, "CLAUDE_CONFIG_DIR"))
        before = snapshot(self.base)
        self.assert_refusal(self.run_command(command), "HOME", self.base)
        self.assertEqual(snapshot(self.base), before)

    def test_automation_logging_refuses_unmarked_parent_of_marked_home(self):
        self.logging_parent("automation-claude.sh")

    def test_execution_logging_refuses_unmarked_parent_of_marked_home(self):
        self.logging_parent("execution-console-claude.sh")

    def symlinked_config(self, case):
        self.marked()
        command = self.prepare_case(case)
        config = self.home / "home/.claude"
        config.rmdir()
        config.symlink_to(self.account, target_is_directory=True)
        before = snapshot(self.base)
        self.assert_refusal(self.run_command(command), "CLAUDE_CONFIG_DIR", self.account)
        self.assertEqual(snapshot(self.base), before)

    def test_shell_provider_refuses_symlinked_config_before_startup(self):
        self.symlinked_config(Case("chat-composer-claude.sh", "CLAUDE_CONFIG_DIR", "version"))

    def test_python_history_writer_refuses_symlinked_config_before_generation(self):
        self.symlinked_config(Case("claude-long-history.py", "HOME", "long-history"))


for _case, _variable, _provided_default in SECONDARY_CASES:
    for _scenario in ("unmarked", "unset", "marked"):
        def _secondary_contract(self, case=_case, variable=_variable,
                                provided_default=_provided_default, scenario=_scenario):
            self.secondary(case, variable, provided_default, scenario)
        setattr(SecondaryPathContracts,
                "test_" + _case.filename.replace(".", "_").replace("-", "_")
                + "_" + _variable.lower() + "_" + _scenario, _secondary_contract)


if __name__ == "__main__":
    unittest.main()
