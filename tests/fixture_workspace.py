#!/usr/bin/env python3
"""Contract tests for scripts/fixture-workspace: reproducibility, the shapes
each edge case promises, the realistic profile's targets, and how `apply`
drives the development executables (stubbed here; the Rust appliers have
their own round-trip tests)."""

from __future__ import annotations

import hashlib
import json
import os
import shutil
import stat
import subprocess
import sys
import tempfile
import textwrap
import unittest
from collections import Counter
from pathlib import Path

REPOSITORY = Path(__file__).resolve().parent.parent
SCRIPT = REPOSITORY / "scripts" / "fixture-workspace"
sys.path.insert(0, str(REPOSITORY / "scripts"))

from fixture_workspace import apply, claude, codex, profiles  # noqa: E402
from fixture_workspace.generate import OWNER, PLAN_FORMAT  # noqa: E402
from fixture_workspace.workspace import Planner  # noqa: E402

AUTHORITY_NAMES = ("CAGENTS_AGENT_AUTHORITY", "CAGENTS_AGENT_TOKEN", "CAGENTS_SESSION_TOKEN")


def run(*arguments, env=None, check=True):
    result = subprocess.run([str(SCRIPT), *arguments], capture_output=True, text=True, env=env)
    if check and result.returncode:
        raise AssertionError(f"{arguments} exited {result.returncode}:\n{result.stderr}{result.stdout}")
    return result


def tree_digest(root):
    """Every path and its content under root, and the modification times of
    the conversation history, which the resume picker orders by. Git's index
    is a stat cache of the checkout, so it is the one file left out."""
    digest = hashlib.sha256()
    history = os.path.join(root, "home")
    for directory, names, files in os.walk(root):
        names.sort()
        for name in sorted(files):
            path = os.path.join(directory, name)
            if name == "index" and f"{os.sep}.git" in path:
                continue
            digest.update(os.path.relpath(path, root).encode() + b"\0")
            if path.startswith(history + os.sep):
                digest.update(str(os.lstat(path).st_mtime_ns).encode() + b"\0")
            with open(path, "rb") as file:
                digest.update(hashlib.sha256(file.read()).digest())
    return digest.hexdigest()


def records(path):
    """A transcript's lines, parsed where they are JSON, else the raw text."""
    with open(path) as file:
        text = file.read()
    parsed = []
    for line in text.split("\n"):
        if not line:
            continue
        try:
            parsed.append(json.loads(line))
        except json.JSONDecodeError:
            parsed.append(line)
    return text, parsed


class Generated(unittest.TestCase):
    """One small workspace with every edge case, shared by the shape tests."""

    @classmethod
    def setUpClass(cls):
        cls.scratch = tempfile.TemporaryDirectory(prefix="fixture-workspace-")
        cls.root = os.path.join(cls.scratch.name, "workspace")
        run("generate", "--profile", "small", "--edge-case", "all", "--root", cls.root)
        with open(os.path.join(cls.root, "plan.json")) as file:
            cls.plan = json.load(file)
        cls.home = cls.plan["home"]

    @classmethod
    def tearDownClass(cls):
        cls.scratch.cleanup()

    def claude_file(self, case):
        entry = self.plan["edge_cases"][case]
        return claude.transcript_path(self.home, entry["cwd"], entry["file_id"])

    def codex_file(self, thread_id):
        (entry,) = [c for c in self.plan["history"]["codex"] if c["thread_id"] == thread_id]
        return entry["file"]

    def test_plan_names_every_edge_case_but_the_explicit_ones(self):
        self.assertEqual(set(self.plan["edge_cases"]), set(profiles.EDGE_CASES) - profiles.EXPLICIT_ONLY)
        self.assertEqual(self.plan["format"], PLAN_FORMAT)

    def test_rust_reads_the_same_plan_format(self):
        source = (REPOSITORY / "src" / "fixture_workspace.rs").read_text()
        self.assertIn(f"pub const PLAN_FORMAT: u32 = {PLAN_FORMAT};", source)

    def test_everything_stays_under_the_root(self):
        root = os.path.realpath(self.root)
        self.assertTrue(self.home.startswith(root + os.sep))
        for entry in self.plan["history"]["claude"] + self.plan["history"]["codex"]:
            self.assertTrue(entry["file"].startswith(root + os.sep), entry["file"])
        for variables in self.plan["environments"].values():
            for name in ("HOME", "XDG_CONFIG_HOME", "XDG_DATA_HOME"):
                value = variables.get(name)
                if value is not None:
                    self.assertTrue(value.startswith(root + os.sep), (name, value))

    def test_intact_claude_transcripts_are_one_session_of_json_records(self):
        damaged = {self.plan["edge_cases"][c]["session_id"] for c in
                   ("malformed-record", "truncated-record", "unfinished-line")}
        forks = {fork["session_id"] for fork in self.plan["edge_cases"]["forked"]}
        for entry in self.plan["history"]["claude"]:
            if entry["session_id"] in damaged | forks:
                continue
            text, parsed = records(entry["file"])
            self.assertTrue(text.endswith("\n"), entry["file"])
            self.assertTrue(all(isinstance(record, dict) for record in parsed), entry["file"])
            sessions = {r["sessionId"] for r in parsed if "sessionId" in r}
            self.assertEqual(sessions, {entry["session_id"]}, entry["file"])
            self.assertEqual(os.path.dirname(entry["file"]),
                             os.path.join(self.home, ".claude", "projects", claude.slug(entry["cwd"])))

    def test_malformed_record_sits_between_intact_ones(self):
        text, parsed = records(self.claude_file("malformed-record"))
        bad = [i for i, record in enumerate(parsed) if not isinstance(record, dict)]
        self.assertEqual(len(bad), 1)
        self.assertTrue(0 < bad[0] < len(parsed) - 1)
        self.assertTrue(text.endswith("\n"))

    def test_truncated_record_is_followed_by_intact_records(self):
        text, parsed = records(self.claude_file("truncated-record"))
        bad = [i for i, record in enumerate(parsed) if not isinstance(record, dict)]
        self.assertEqual(len(bad), 1)
        self.assertTrue(parsed[bad[0]].startswith("{"))
        self.assertTrue(isinstance(parsed[-1], dict))
        self.assertTrue(text.endswith("\n"))

    def test_unfinished_line_is_last_and_has_no_newline(self):
        text, parsed = records(self.claude_file("unfinished-line"))
        self.assertFalse(text.endswith("\n"))
        self.assertIsInstance(parsed[-1], str)
        self.assertTrue(all(isinstance(record, dict) for record in parsed[:-1]))

    def test_renamed_file_keeps_its_session_id_inside(self):
        entry = self.plan["edge_cases"]["renamed-file"]
        self.assertNotEqual(entry["file_id"], entry["session_id"])
        _, parsed = records(self.claude_file("renamed-file"))
        self.assertEqual({r["sessionId"] for r in parsed if "sessionId" in r}, {entry["session_id"]})

    def test_sidechain_transcript_mixes_main_and_sidechain_records(self):
        _, parsed = records(self.claude_file("sidechain"))
        chains = Counter(r.get("isSidechain") for r in parsed if r.get("type") in ("user", "assistant"))
        self.assertGreater(chains[True], 0)
        self.assertGreater(chains[False], 0)

    def test_fork_starts_with_its_origin_and_lives_in_another_project(self):
        forks = self.plan["edge_cases"]["forked"]
        self.assertTrue(forks)
        by_session = {e["session_id"]: e for e in self.plan["history"]["claude"]}
        for fork in forks:
            self.assertNotEqual(claude.slug(fork["cwd"]), claude.slug(fork["origin_cwd"]))
            _, parsed = records(by_session[fork["session_id"]]["file"])
            _, origin = records(by_session[fork["origin"]]["file"])
            sessions = [r["sessionId"] for r in parsed if "sessionId" in r]
            self.assertEqual(sessions[0], fork["origin"])
            self.assertEqual(sessions[-1], fork["session_id"])
            switch = sessions.index(fork["session_id"])
            self.assertEqual(set(sessions[:switch]), {fork["origin"]})
            self.assertEqual(set(sessions[switch:]), {fork["session_id"]})
            # The prefix is the origin's own records, verbatim.
            prefix = [r for r in parsed if r.get("sessionId") == fork["origin"]]
            self.assertEqual(prefix, [r for r in origin if r.get("sessionId") == fork["origin"]][: len(prefix)])
            self.assertTrue(all(r["cwd"] == fork["origin_cwd"] for r in prefix if "cwd" in r))

    def test_long_project_path_uses_a_hashed_prefix_folder(self):
        name = self.plan["edge_cases"]["long-project-path"]["project"]
        (project,) = [p for p in self.plan["projects"] if p["name"] == name]
        self.assertGreater(len(project["checkout"]), 200)
        folder = claude.slug(project["checkout"])
        self.assertTrue(folder.startswith(claude.slug(project["checkout"])[:200]))
        self.assertNotEqual(folder, "".join(c if c.isascii() and c.isalnum() else "-" for c in project["checkout"]))
        files = [e for e in self.plan["history"]["claude"] if e["cwd"] == project["checkout"]]
        for entry in files:
            self.assertEqual(os.path.basename(os.path.dirname(entry["file"])), folder)
        self.assertTrue(any(e["cwd"].startswith(project["checkout"]) for e in self.plan["history"]["claude"]))

    def test_removed_worktree_is_gone_but_still_planned(self):
        path = self.plan["edge_cases"]["removed-worktree"]["path"]
        self.assertFalse(os.path.exists(path))
        worktrees = [w for p in self.plan["projects"] for w in p["worktrees"] if w["path"] == path]
        self.assertEqual([w["removed"] for w in worktrees], [True])
        listed = subprocess.run(["git", "-C", os.path.dirname(os.path.dirname(path)), "worktree", "list"],
                                capture_output=True, text=True, check=True).stdout
        self.assertNotIn(path, listed)
        self.assertTrue(any(v["scope"].get("worktree", {}).get("path") == path for v in self.plan["views"]))

    def test_codex_turn_endings(self):
        _, parsed = records(self.codex_file(self.plan["edge_cases"]["codex-interrupted-turn"]["thread_id"]))
        events = [r["payload"]["type"] for r in parsed if r["type"] == "event_msg"]
        self.assertEqual(events[-1], "turn_aborted")
        self.assertEqual(parsed[-1]["payload"]["reason"], "interrupted")

        _, parsed = records(self.codex_file(self.plan["edge_cases"]["codex-usage-limit"]["thread_id"]))
        last = parsed[-1]["payload"]
        self.assertEqual(last["type"], "task_complete")
        self.assertIsNone(last["last_agent_message"])
        self.assertIn("usage_limit", json.dumps(last))

    def test_archived_codex_rollouts_are_moved(self):
        archived = self.plan["edge_cases"]["codex-archived"]
        self.assertTrue(archived)
        for thread_id in archived:
            self.assertIn(f"{os.sep}archived_sessions{os.sep}", self.codex_file(thread_id))

    def test_codex_rollouts_open_with_session_meta(self):
        for entry in self.plan["history"]["codex"]:
            text, parsed = records(entry["file"])
            self.assertTrue(text.endswith("\n"))
            self.assertTrue(all(isinstance(record, dict) for record in parsed), entry["file"])
            meta = parsed[0]
            self.assertEqual(meta["type"], "session_meta")
            self.assertEqual(meta["payload"]["id"], entry["thread_id"])
            starts = sum(r["type"] == "event_msg" and r["payload"]["type"] == "task_started" for r in parsed)
            self.assertEqual(starts, entry["turns"])
            if entry["subagent"]:
                spawn = meta["payload"]["source"]["subagent"]["thread_spawn"]
                self.assertEqual(spawn["depth"], 1)
                self.assertTrue(spawn["agent_path"].startswith("/root/"))
                self.assertEqual(spawn["agent_role"], "default")

    def test_session_index_names_only_top_level_threads(self):
        index = os.path.join(self.home, ".codex", "session_index.jsonl")
        _, parsed = records(index)
        subagents = {c["thread_id"] for c in self.plan["history"]["codex"] if c["subagent"]}
        self.assertTrue(parsed)
        self.assertFalse({r["id"] for r in parsed} & subagents)

    def test_cards_views_and_windows_agree(self):
        cards = {card["id"]: card for card in self.plan["cards"]}
        for view in self.plan["views"]:
            for pane in view["panes"]:
                self.assertIsNone(cards[pane]["closed_at_ms"])
            for retired in view["retired"]:
                self.assertIsNotNone(cards[retired]["closed_at_ms"])
            self.assertLessEqual(len(view["panes"]), 8)
        channel_cards = Counter(card for channel in self.plan["channels"] for card in channel["cards"])
        self.assertTrue(all(count == 1 for count in channel_cards.values()))
        for window in self.plan["windows"]:
            held = set(window["cards"])
            self.assertTrue(held <= set(cards))
            if window["focused"] is not None:
                self.assertIn(window["focused"], held)
                self.assertIsNone(cards[window["focused"]]["closed_at_ms"])
            for unread in window["unread"]:
                self.assertIsNone(cards[unread["card"]]["closed_at_ms"])
        sessions = {e["session_id"] for e in self.plan["history"]["claude"]}
        threads = {e["thread_id"] for e in self.plan["history"]["codex"] if not e["subagent"]}
        for card in cards.values():
            kind = card["kind"]
            if kind["type"] == "chat" and kind.get("session_id"):
                self.assertIn(kind["session_id"], sessions | threads)

    def test_avatars_are_pngs_for_github_projects(self):
        for project in self.plan["projects"]:
            if project["avatar"]:
                self.assertIn("github.com", project["remote"])
                with open(os.path.join(self.root, "avatars", f"{project['avatar']['owner']}.png"), "rb") as file:
                    self.assertEqual(file.read(8), b"\x89PNG\r\n\x1a\n")


class Reproducible(unittest.TestCase):
    def setUp(self):
        self.scratch = tempfile.TemporaryDirectory(prefix="fixture-workspace-")
        self.addCleanup(self.scratch.cleanup)
        self.root = os.path.join(self.scratch.name, "workspace")

    def test_same_seed_writes_the_same_tree(self):
        run("generate", "--profile", "small", "--edge-case", "all", "--root", self.root)
        first = tree_digest(self.root)
        run("generate", "--profile", "small", "--edge-case", "all", "--root", self.root, "--replace")
        self.assertEqual(tree_digest(self.root), first)
        run("generate", "--profile", "small", "--edge-case", "all", "--seed", "2", "--root", self.root, "--replace")
        self.assertNotEqual(tree_digest(self.root), first)

    def test_repository_history_is_reproducible(self):
        run("generate", "--profile", "small", "--root", self.root)
        with open(os.path.join(self.root, "plan.json")) as file:
            checkout = json.load(file)["projects"][0]["checkout"]
        head = subprocess.run(["git", "-C", checkout, "rev-parse", "HEAD"], capture_output=True, text=True,
                              check=True).stdout
        run("generate", "--profile", "small", "--root", self.root, "--replace")
        self.assertEqual(subprocess.run(["git", "-C", checkout, "rev-parse", "HEAD"], capture_output=True,
                                        text=True, check=True).stdout, head)

    def test_replace_refuses_a_directory_it_did_not_generate(self):
        os.makedirs(self.root)
        Path(self.root, "keep.txt").write_text("not ours")
        result = run("generate", "--root", self.root, "--replace", check=False)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn(OWNER, result.stderr)
        self.assertTrue(Path(self.root, "keep.txt").exists())
        result = run("generate", "--root", self.root, check=False)
        self.assertIn("--replace", result.stderr)

    def test_no_edge_cases_writes_none(self):
        run("generate", "--profile", "small", "--no-edge-cases", "--root", self.root)
        with open(os.path.join(self.root, "plan.json")) as file:
            self.assertEqual(json.load(file)["edge_cases"], {})

    def test_unknown_names_are_refused_with_the_choices(self):
        result = run("generate", "--profile", "nope", "--root", self.root, check=False)
        self.assertIn("small", result.stderr)
        result = run("generate", "--edge-case", "nope", "--root", self.root, check=False)
        self.assertIn("malformed-record", result.stderr)


class Profiles(unittest.TestCase):
    """Plans the large profiles without writing them."""

    def plan(self, name, **changes):
        profile = profiles.resolve(name, **changes)
        return Planner(profile, 1, "/nonexistent/fixture").plan()

    def test_each_edge_case_alone_is_the_only_one_planned(self):
        for case in profiles.EDGE_CASES:
            if case in profiles.EXPLICIT_ONLY:
                continue
            with self.subTest(case=case):
                planner = self.plan("small", add=[case], drop_all=True)
                self.assertEqual(set(planner.edge_cases), {case})

    def test_oversized_transcript_is_only_planned_when_named(self):
        self.assertNotIn("oversized-transcript", self.plan("small", add=["all"]).edge_cases)
        for profile in profiles.PROFILES.values():
            self.assertNotIn("oversized-transcript", profile.edge_cases)
        planner = self.plan("small", add=["oversized-transcript"], drop_all=True)
        (spec,) = [s for s in planner.claude if s.get("edge") == "oversized-transcript"]
        self.assertGreater(spec["pad_mib"], 256)

    def test_realistic_matches_the_measured_workspace(self):
        planner = self.plan("realistic")
        self.assertEqual(len(planner.projects), 18)
        self.assertEqual(len(planner.channels), 76)
        scopes = Counter(next(iter(view["scope"])) for view in planner.views)
        self.assertEqual(scopes, {"channel": 76, "worktree": 1780})
        shown = [view for view in planner.views if view["panes"]]
        self.assertEqual(len(shown), 44)
        cards = {card["id"]: card for card in planner.cards}
        panes = Counter(cards[pane]["kind"]["type"] for view in shown for pane in view["panes"])
        self.assertEqual(panes, {"chat": 52, "terminal": 27, "repository": 4})
        self.assertLessEqual(max(len(view["panes"]) for view in shown), 8)
        retired = [len(view["retired"]) for view in planner.views if view["retired"]]
        self.assertEqual(len(retired), 89)
        self.assertEqual(max(retired), 63)
        edits = sorted((view["edits"] for view in planner.views), reverse=True)
        self.assertEqual(sum(edits), 45970)
        self.assertEqual(edits[:3], [36608, 5891, 1320])
        self.assertEqual(len(planner.cards), sum(profiles.PROFILES["realistic"].cards.values()))
        self.assertEqual(Counter(card["kind"]["type"] for card in planner.cards),
                         profiles.PROFILES["realistic"].cards)
        closed = sum(card["closed_at_ms"] is not None for card in planner.cards) / len(planner.cards)
        self.assertGreater(closed, 0.93)
        (_, second) = planner.windows
        self.assertEqual(len(second["cards"]), 1709)

    def test_sidebar_profile_has_its_static_states(self):
        planner = self.plan("sidebar")
        self.assertTrue(8 <= len(planner.projects) <= 12)
        self.assertTrue(any(p["avatar"] for p in planner.projects))
        self.assertTrue(any(not p["remote"] for p in planner.projects))
        self.assertTrue(any(p["dirty"] for p in planner.projects))
        unread = Counter()
        for entry in planner.windows[0]["unread"]:
            unread[json.dumps(entry["owner"])] += 1
        self.assertEqual(sorted(unread.values()), [3, 12, 120])

    def test_scale_multiplies_counts_but_not_structure(self):
        base = profiles.resolve("small")
        doubled = profiles.resolve("small", scale=2)
        self.assertEqual(doubled.projects, base.projects)
        self.assertEqual(doubled.claude_transcripts, base.claude_transcripts * 2)


STUB = """\
#!/usr/bin/env python3
import json, os, sys
log = os.environ["STUB_LOG"]
runtime = os.environ.get("XDG_RUNTIME_DIR")
with open(log, "a") as file:
    file.write(json.dumps({{"argv": [os.path.basename(sys.argv[0])] + sys.argv[1:],
                           "env": {{k: os.environ.get(k) for k in {names!r}}},
                           "runtime_mode": os.stat(runtime).st_mode & 0o777 if runtime else None}}) + "\\n")
if os.path.basename(sys.argv[0]) == "cagentsd":
    print(json.dumps({{"path": "/x/workspace-model.sqlite3", "identity": "6f9c2d0e-0000-4000-8000-000000000001",
                      "schema": 1, "projects": 3, "channels": 6, "views": 18, "panes": 9, "edits": 40,
                      "seconds": 0.1}}))
else:
    print(json.dumps({{"path": "/x/session.sqlite3", "slot": sys.argv[4], "schema": 13, "cards": 1, "open": 1,
                      "channels": 6, "projects": 3, "seconds": 0.1}}))
"""

WATCHED = ("HOME", "XDG_DATA_HOME", "XDG_CONFIG_HOME", "XDG_CACHE_HOME", "XDG_STATE_HOME", "XDG_RUNTIME_DIR",
           "CAGENTS_DEVELOPMENT_INSTANCE") + AUTHORITY_NAMES


class Apply(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.scratch = tempfile.TemporaryDirectory(prefix="fixture-workspace-")
        cls.root = os.path.join(cls.scratch.name, "workspace")
        run("generate", "--profile", "small", "--root", cls.root)
        cls.bin = os.path.join(cls.scratch.name, "bin")
        os.makedirs(cls.bin)
        for name in ("cagentsd", "cagents-ui"):
            path = os.path.join(cls.bin, name)
            Path(path).write_text(STUB.format(names=WATCHED))
            os.chmod(path, os.stat(path).st_mode | stat.S_IXUSR)

    @classmethod
    def tearDownClass(cls):
        cls.scratch.cleanup()

    def apply(self, *arguments, extra=None):
        log = os.path.join(self.scratch.name, "log")
        if os.path.exists(log):
            os.remove(log)
        env = dict(os.environ, STUB_LOG=log, CAGENTS_DEVELOPMENT_INSTANCE="leaked", **(extra or {}))
        result = run("apply", "--root", self.root, "--bin-dir", self.bin, *arguments, env=env, check=False)
        calls = [json.loads(line) for line in Path(log).read_text().splitlines()] if os.path.exists(log) else []
        return result, calls

    def test_daemon_first_then_each_window_with_its_identity(self):
        result, calls = self.apply()
        self.assertEqual(result.returncode, 0, result.stderr)
        plan = os.path.join(os.path.realpath(self.root), "plan.json")
        self.assertEqual(calls[0]["argv"], ["cagentsd", "--development-seed-workspace", plan])
        slots = [w["slot"] for w in json.loads(Path(plan).read_text())["windows"]]
        self.assertEqual([c["argv"] for c in calls[1:]],
                         [["cagents-ui", "--development-seed-workspace", plan, "--window", slot,
                           "--workspace-model-identity", "6f9c2d0e-0000-4000-8000-000000000001"] for slot in slots])

    def test_without_an_instance_every_store_is_inside_the_root(self):
        _, calls = self.apply()
        root = os.path.realpath(self.root)
        for call in calls:
            env = call["env"]
            self.assertIsNone(env["CAGENTS_DEVELOPMENT_INSTANCE"])
            for name in WATCHED[:6]:
                self.assertTrue(env[name].startswith(root + os.sep), (name, env[name]))
            self.assertEqual(call["runtime_mode"], 0o700)
        # Each seed's runtime directory is its own, gone once it finishes.
        self.assertEqual(len({call["env"]["XDG_RUNTIME_DIR"] for call in calls}), 1)
        self.assertFalse(os.path.exists(calls[0]["env"]["XDG_RUNTIME_DIR"]))

    def test_an_instance_keeps_the_callers_directories(self):
        _, calls = self.apply("--instance", "seeded")
        for call in calls:
            self.assertEqual(call["env"]["CAGENTS_DEVELOPMENT_INSTANCE"], "seeded")
            self.assertEqual(call["env"]["HOME"], os.environ.get("HOME"))

    def test_the_unnamed_instance_is_refused(self):
        for name in ("", "../x", "a b", "a_b", "-a", "x" * 25):
            result, calls = self.apply("--instance", name)
            self.assertNotEqual(result.returncode, 0)
            self.assertEqual(calls, [])

    def test_agent_authority_is_passed_through_unchanged(self):
        # The executables refuse to seed on their own terms; apply never strips
        # or invents an authority to get past them.
        _, calls = self.apply(extra={"CAGENTS_AGENT_AUTHORITY": "kept"})
        self.assertTrue(all(call["env"]["CAGENTS_AGENT_AUTHORITY"] == "kept" for call in calls))
        _, calls = self.apply()
        expected = os.environ.get("CAGENTS_AGENT_AUTHORITY")
        self.assertTrue(all(call["env"]["CAGENTS_AGENT_AUTHORITY"] == expected for call in calls))

    def test_a_failing_daemon_stops_before_any_window(self):
        failing = os.path.join(self.scratch.name, "failing")
        os.makedirs(failing, exist_ok=True)
        for name in ("cagentsd", "cagents-ui"):
            path = os.path.join(failing, name)
            Path(path).write_text(textwrap.dedent("""\
                #!/bin/sh
                echo "A cagents daemon is running for this instance" >&2
                exit 1
            """) if name == "cagentsd" else "#!/bin/sh\ntouch \"$STUB_LOG.window\"\n")
            os.chmod(path, 0o755)
        log = os.path.join(self.scratch.name, "failing-log")
        result = run("apply", "--root", self.root, "--bin-dir", failing, env=dict(os.environ, STUB_LOG=log),
                     check=False)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("daemon is running", result.stderr)
        self.assertFalse(os.path.exists(log + ".window"))

    def test_a_plan_moved_from_where_it_was_generated_is_refused(self):
        moved = os.path.join(self.scratch.name, "moved")
        shutil.copytree(self.root, moved, symlinks=True)
        self.addCleanup(shutil.rmtree, moved)
        result = run("apply", "--root", moved, "--bin-dir", self.bin, check=False)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("Generate it again", result.stderr)

    def test_missing_executables_name_the_fix(self):
        result = run("apply", "--root", self.root, "--bin-dir", self.scratch.name, check=False)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("dev-runtime", result.stderr)


class WriterLocks(unittest.TestCase):
    def test_held_locks_are_exclusive_flocks(self):
        import fcntl

        with tempfile.TemporaryDirectory(prefix="fixture-workspace-") as home:
            thread = "019e0000-0000-7000-8000-000000000001"
            held = codex.hold_writer_locks(home, [thread])
            try:
                with open(codex.lock_path(home, thread), "a") as other:
                    with self.assertRaises(BlockingIOError):
                        fcntl.flock(other, fcntl.LOCK_EX | fcntl.LOCK_NB)
            finally:
                for file in held:
                    file.close()


if __name__ == "__main__":
    unittest.main()
