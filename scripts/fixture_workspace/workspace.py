"""Plans the workspace: projects, worktrees, cards, channels, shared views,
windows and the conversations behind them.

The plan is intent at the level of the fixture, not of the stores' types: the
Rust appliers turn it into the real records through the real writers. Every
choice is drawn from a named stream (see determinism.py), so changing how one
part is planned does not reshuffle the others.

Open cards that can be panes are each shown in exactly one shared view, as in
a real install; closed cards are history, some of them retired from a view.
"""

import os

from . import content
from .determinism import EPOCH, Streams

DAY = 86400
UTILITY = ("settings", "accounts", "automations", "work")
TITLE_SOURCES = {"Prompt": 6, "Provider": 2, "Manual": 2}
LONG_PATH_PARENT = "a-very-deeply-nested-directory-that-someone-cloned-into-for-a-client-engagement"


class Planner:
    def __init__(self, profile, seed, root, epoch=EPOCH):
        self.profile = profile
        self.seed = seed
        self.root = root
        self.home = os.path.join(root, "home")
        self.epoch = epoch
        self.streams = Streams(seed)
        self.edge = profile.edge_cases
        self.projects = []
        self.cards = []
        self.channels = []
        self.views = []
        self.claude = []  # Conversation specs.
        self.codex = []
        self.edge_cases = {}

    def ago(self, stream, days):
        """A time before the epoch, mostly recent."""
        return self.epoch - days * DAY * stream.random() ** 2 - stream.uniform(60, 3600)

    # Projects and worktrees.

    def plan_projects(self):
        stream = self.streams("projects")
        profile = self.profile
        names = content.PROJECT_NAMES[: profile.projects]
        for index, name in enumerate(names):
            parent = os.path.join(self.root, "projects")
            if "long-project-path" in self.edge and index == len(names) - 1:
                parent = os.path.join(parent, *([LONG_PATH_PARENT] * 3))
                self.edge_cases["long-project-path"] = {"project": name}
            checkout = os.path.join(parent, name)
            project = {
                "id": stream.uuid4(),
                "name": name,
                "checkout": checkout,
                "repository": os.path.join(checkout, ".git"),
                "commits": stream.randint(3, 30),
                "worktrees": [{"id": stream.uuid4(), "path": checkout, "branch": "main", "removed": False,
                               "main": True}],
                "historical": [],
                "remote": None,
                "avatar": None,
                "dirty": [],
            }
            if index < profile.avatars:
                owner = f"cagents-fixture-{name}"
                project["remote"] = f"https://github.com/{owner}/{name}.git"
                project["avatar"] = {"owner": owner, "color": [stream.randint(40, 220) for _ in range(3)]}
            self.projects.append(project)
        branches = list(content.BRANCH_WORDS)
        stream.shuffle(branches)
        used = {}

        def branch_for(project):
            word = branches[used.setdefault(project["name"], 0) % len(branches)]
            used[project["name"]] += 1
            return word if used[project["name"]] <= len(branches) else f"{word}-{used[project['name']]}"

        removed = profile.removed_worktrees if "removed-worktree" in self.edge else 0
        for count, is_removed in ((profile.live_worktrees, False), (removed, True)):
            for _ in range(count):
                # Most worktrees belong to the projects people work in most.
                project = self.projects[stream.skewed(0, len(self.projects) - 1, 1.8)]
                branch = branch_for(project)
                project["worktrees"].append({
                    "id": stream.uuid4(), "path": os.path.join(project["checkout"], ".worktrees", branch),
                    "branch": branch, "removed": is_removed, "main": False})
        if removed:
            gone = next(w for p in self.projects for w in p["worktrees"] if w["removed"])
            self.edge_cases["removed-worktree"] = {"path": gone["path"]}
        live = [w for p in self.projects for w in p["worktrees"] if not w["main"] and not w["removed"]]
        for worktree in live[: profile.dirty_worktrees]:
            project = next(p for p in self.projects if worktree in p["worktrees"])
            project["dirty"].append(worktree["path"])
        # Worktrees long since deleted that still have a shared view.
        known = sum(len(p["worktrees"]) for p in self.projects)
        for number in range(max(0, profile.historical_worktree_scopes - known)):
            project = self.projects[stream.skewed(0, len(self.projects) - 1, 3)]
            project["historical"].append(os.path.join(project["checkout"], ".worktrees", f"task-{number + 1}"))

    def live_worktrees(self, project):
        return [w for w in project["worktrees"] if not w["removed"]]

    # Cards.

    def environment(self, stream, size):
        """A launch's inherited environment. It points at the fixture's own
        home, provider homes and state, never the person's, and carries no
        display or session bus address. The app layers it over its own
        environment, so the provider homes are named here even though they are
        the defaults under HOME; the appliers also unset provider credentials."""
        base = {
            "HOME": self.home,
            "CLAUDE_CONFIG_DIR": f"{self.home}/.claude",
            "CODEX_HOME": f"{self.home}/.codex",
            "PATH": f"{self.root}/bin:/usr/local/bin:/usr/bin:/bin",
            "LANG": "en_US.UTF-8",
            "TERM": "xterm-256color",
            "COLORTERM": "truecolor",
            "SHELL": "/bin/bash",
            "USER": "fixture",
            "LOGNAME": "fixture",
            "XDG_DATA_HOME": f"{self.root}/data",
            "XDG_CONFIG_HOME": f"{self.root}/config",
            "XDG_CACHE_HOME": f"{self.root}/cache",
            "XDG_STATE_HOME": f"{self.root}/state",
            "EDITOR": "vi",
            "PAGER": "less",
            "LESS": "-R",
        }
        extra = [
            ("CARGO_HOME", f"{self.home}/.cargo"), ("RUSTUP_HOME", f"{self.home}/.rustup"),
            ("GOPATH", f"{self.home}/go"), ("NODE_OPTIONS", "--max-old-space-size=4096"),
            ("NPM_CONFIG_PREFIX", f"{self.home}/.npm-global"), ("PYTHONDONTWRITEBYTECODE", "1"),
            ("RUST_BACKTRACE", "1"), ("CARGO_TERM_COLOR", "always"), ("HISTSIZE", "100000"),
            ("MANPAGER", "less -R"), ("FZF_DEFAULT_OPTS", "--height 40% --layout=reverse --border"),
            ("LS_COLORS", "rs=0:di=01;34:ln=01;36:mh=00:pi=40;33:so=01;35:do=01;35:bd=40;33;01:cd=40;33;01:"
                          "or=40;31;01:mi=00:su=37;41:sg=30;43:ca=00:tw=30;42:ow=34;42:st=37;44:ex=01;32"),
            ("GIT_PAGER", "less -FRX"), ("DOCKER_HOST", f"unix://{self.root}/state/docker.sock"),
            ("JAVA_HOME", "/usr/lib/jvm/default"), ("GOFLAGS", "-mod=mod"), ("NO_COLOR", ""),
            ("PNPM_HOME", f"{self.home}/.local/share/pnpm"), ("VIRTUAL_ENV_DISABLE_PROMPT", "1"),
            ("PROMPT_COMMAND", "history -a; history -n"), ("PIP_REQUIRE_VIRTUALENV", "true"),
            ("BUN_INSTALL", f"{self.home}/.bun"), ("DENO_INSTALL", f"{self.home}/.deno"),
        ]
        for key, value in extra[:size]:
            base[key] = value
        return base

    def plan_environments(self):
        stream = self.streams("environments")
        # A handful of shells, each with a different amount of inherited state.
        self.environments = {f"shell-{size}": self.environment(stream, size) for size in (0, 6, 12, 18, 25)}

    def new_card(self, stream, kind, project, worktree, closed_at=None):
        card = {
            "id": stream.uuid4(),
            "title": "",
            "width": stream.choice([0.33, 0.4, 0.5, 0.5, 0.5, 0.6, 0.66, 1.0]),
            "closed_at_ms": None if closed_at is None else int(closed_at * 1000),
            "scope": worktree,
            "project": project["id"] if project else None,
            "kind": {"type": kind},
        }
        details = card["kind"]
        environment = stream.choice(sorted(self.environments))
        if kind == "chat":
            provider = "claude" if stream.random() < 0.8 else "codex"
            details.update(provider=provider, terminal_id=stream.uuid4(), session_id=None, cwd=worktree,
                           program=provider, environment=environment,
                           arguments=["--model", "haiku"] if provider == "claude" else ["-m", "gpt-5.2-codex"])
            card["title"] = stream.choice(content.TOPICS)[:60]
        elif kind == "terminal":
            command = stream.choice(["", "", "cargo test", "npm run dev", "htop", "git log --oneline"])
            details.update(terminal_id=stream.uuid4(), cwd=worktree, program="/bin/bash",
                           arguments=["-l"] if not command else ["-lc", command], environment=environment)
            card["title"] = command or "bash"
        elif kind == "repository":
            details.update(project=project["id"], repository=project["repository"], worktree=worktree,
                           selected_file=stream.choice([None, None, "src/lib.rs", "README.md"]))
            card["title"] = project["name"]
        elif kind == "pull_requests":
            details.update(repository=project["repository"], selected_number=stream.choice([None, 42, 117]))
            card["title"] = "Pull requests"
        elif kind == "browser":
            url = stream.choice(["http://localhost:3000/", "http://localhost:5173/dashboard", "https://example.com/",
                                 "https://docs.rs/tokio/latest/tokio/", "http://127.0.0.1:8080/health"])
            details.update(browser_id=stream.uuid4(), url=url)
            card["title"] = url.split("//", 1)[1].rstrip("/")
        else:
            card["title"] = kind.replace("_", " ").title()
        self.cards.append(card)
        return card

    def plan_views_and_cards(self):
        stream = self.streams("cards")
        profile = self.profile
        projects = self.projects
        # Channels, most of them in the busiest projects.
        channel_stream = self.streams("channels")
        for _ in range(profile.channels):
            project = projects[channel_stream.skewed(0, len(projects) - 1, 1.6)]
            self.channels.append({
                "id": channel_stream.uuid4(), "project": project["id"],
                "title": channel_stream.choice(content.TOPICS)[:48],
                "title_source": channel_stream.weighted(TITLE_SOURCES), "cards": [], "focused": None})
        by_project = {p["id"]: p for p in projects}
        channel_views = [{"scope": {"channel": c["id"]}, "project": by_project[c["project"]], "channel": c,
                          "panes": [], "retired": [], "edits": 0} for c in self.channels]
        worktree_views = []
        for project in projects:
            for worktree in project["worktrees"]:
                worktree_views.append({"scope": {"worktree": {"repository": project["repository"],
                                                              "path": worktree["path"]}},
                                       "project": project, "worktree": worktree, "panes": [], "retired": [],
                                       "edits": 0})
            for path in project["historical"]:
                worktree_views.append({"scope": {"worktree": {"repository": project["repository"], "path": path}},
                                       "project": project, "worktree": None, "panes": [], "retired": [],
                                       "edits": 0})
        self.views = channel_views + worktree_views

        # Which views show panes: mostly channels, a few live worktrees.
        live_views = [v for v in worktree_views if v["worktree"] and not v["worktree"]["removed"]]
        nonempty = min(profile.nonempty_views, len(channel_views) + len(live_views))
        from_worktrees = min(len(live_views), max(1, nonempty // 6))
        shown = stream.sample(channel_views, nonempty - from_worktrees) + stream.sample(live_views, from_worktrees)
        kinds = [kind for kind, count in profile.pane_mix.items() for _ in range(count)]
        stream.shuffle(kinds)
        # Unread conversations need their project's views to hold enough chats.
        demands = list(profile.unread)
        unread_cards = []
        # Every shown view has a pane; the rest go mostly to a few busy views,
        # never more than eight to a view.
        slots = {id(v): [kinds.pop() if kinds else "chat"] for v in shown}
        while kinds:
            room = [v for v in shown if len(slots[id(v)]) < 8] or shown
            slots[id(room[stream.skewed(0, len(room) - 1, 3)])].append(kinds.pop())
        for view in shown:
            project = view["project"]
            live = self.live_worktrees(project)
            for kind in slots[id(view)]:
                worktree = view["worktree"]["path"] if view.get("worktree") else stream.choice(live)["path"]
                card = self.new_card(stream, kind, project, worktree)
                view["panes"].append(card["id"])
                if view.get("channel"):
                    view["channel"]["cards"].append(card["id"])
            if view.get("channel") and view["panes"]:
                view["channel"]["focused"] = view["panes"][0]
        for demand, project in zip(demands, projects):
            chats = [c for c in self.cards if c["project"] == project["id"] and c["kind"]["type"] == "chat"]
            while len(chats) < demand:
                # A project needs more open conversations than its views
                # were given: add them to its root worktree's view.
                root_view = next(v for v in worktree_views if v["worktree"] is project["worktrees"][0])
                card = self.new_card(stream, "chat", project, project["checkout"])
                root_view["panes"].append(card["id"])
                chats.append(card)
            unread_cards.append({"project": project["id"], "worktree": project["worktrees"][0]["id"],
                                 "cards": [c["id"] for c in chats[:demand]]})
        self.unread = unread_cards
        # Open utilities, which are never panes.
        for kind in UTILITY:
            if profile.cards.get(kind):
                self.new_card(stream, kind, None, None)
        # Everything else ever opened, now closed.
        opened = {}
        for card in self.cards:
            opened[card["kind"]["type"]] = opened.get(card["kind"]["type"], 0) + 1
        everywhere = [(p, w["path"]) for p in projects for w in p["worktrees"]]
        everywhere += [(p, path) for p in projects for path in p["historical"][:40]]
        closed = []
        for kind, count in profile.cards.items():
            for _ in range(max(0, count - opened.get(kind, 0))):
                project, worktree = everywhere[stream.skewed(0, len(everywhere) - 1, 1.3)]
                utility = kind in UTILITY
                closed.append(self.new_card(stream, kind, None if utility else project,
                                            None if utility else worktree, closed_at=self.ago(stream, 120)))
        # Panes closed out of a view stay retired from it.
        retiring = [c for c in closed if c["kind"]["type"] in ("chat", "terminal")]
        stream.shuffle(retiring)
        candidates = [v for v in self.views if v.get("channel") or (v["worktree"] and not v["worktree"]["main"])]
        for number, view in enumerate(stream.sample(candidates, min(len(candidates), profile.retired_views))):
            size = profile.retired_max if number == 0 else stream.skewed(1, max(1, profile.retired_max // 5), 2)
            for _ in range(size):
                if not retiring:
                    break
                card = retiring.pop()
                card["scope"] = self.view_path(view, card["scope"])
                view["retired"].append(card["id"])
        # Recorded edits: a few views took almost all of them.
        edit_stream = self.streams("edits")
        if profile.view_edits:
            heavy = sorted((v for v in self.views if v["panes"]), key=lambda v: -len(v["panes"]))
            for view, count in zip(heavy, profile.view_edit_outliers):
                view["edits"] = count
            rest = profile.view_edits - sum(v["edits"] for v in self.views)
            light = edit_stream.sample([v for v in self.views if not v["edits"]], min(102, len(self.views) - 3))
            for index, view in enumerate(light):
                share = rest // (len(light) - index) if index < len(light) - 1 else rest
                share = max(1, min(rest, int(share * edit_stream.uniform(0.2, 1.8)))) if index < len(light) - 1 else rest
                view["edits"] = share
                rest -= share
                if rest <= 0:
                    break

    def view_path(self, view, fallback):
        if view.get("worktree"):
            return view["worktree"]["path"]
        return fallback

    # Conversations.

    def prompts(self, stream):
        turns = stream.randint(*self.profile.turns)
        return [stream.choice(content.TOPICS)] + [stream.choice(content.FOLLOW_UPS) for _ in range(turns - 1)]

    def turn_set(self, stream, prompts, fraction):
        return sorted(i for i in range(len(prompts)) if stream.random() < fraction)

    def branch_of(self, path):
        for project in self.projects:
            for worktree in project["worktrees"]:
                if worktree["path"] == path:
                    return worktree["branch"]
        return os.path.basename(path)

    def claude_spec(self, stream, cwd, ended_days, recent=False):
        prompts = self.prompts(stream)
        started = self.epoch - (stream.uniform(0.02, 0.5) * DAY if recent else ended_days * DAY * stream.random() ** 2)
        started -= len(prompts) * 300
        spec = {
            "session_id": stream.uuid4(), "cwd": cwd, "branch": self.branch_of(cwd), "started_at": started,
            "prompts": prompts, "long_turns": self.turn_set(stream, prompts, self.profile.long_turn_fraction),
            "subagent_turns": self.turn_set(stream, prompts, self.profile.subagent_fraction / max(1, len(prompts) / 2)),
            "noise": [], "title": None, "edge": None,
        }
        spec["file_id"] = spec["session_id"]
        if stream.random() < self.profile.noise_fraction:
            spec["noise"] = stream.sample(sorted(content.NOISE), stream.randint(1, 3))
        if stream.random() < self.profile.titled_fraction:
            kind = "custom-title" if stream.random() < 0.3 else "ai-title"
            spec["title"] = (kind, prompts[0][:48])
        self.claude.append(spec)
        return spec

    def codex_spec(self, stream, cwd, ended_days, recent=False):
        prompts = self.prompts(stream)
        started = self.epoch - (stream.uniform(0.02, 0.5) * DAY if recent else ended_days * DAY * stream.random() ** 2)
        started -= len(prompts) * 300
        spec = {
            "thread_id": stream.uuid7(int(started * 1000)), "cwd": cwd, "branch": self.branch_of(cwd),
            "started_at": started, "prompts": prompts,
            "long_turns": self.turn_set(stream, prompts, self.profile.long_turn_fraction), "endings": {},
            "name": prompts[0][:40] if stream.random() < self.profile.titled_fraction / 2 else None,
        }
        self.codex.append(spec)
        return spec

    def plan_history(self):
        stream = self.streams("history")
        profile = self.profile
        chats = [c for c in self.cards if c["kind"]["type"] == "chat"]
        # Every open conversation and most closed ones have their history.
        for card in chats:
            details = card["kind"]
            is_open = card["closed_at_ms"] is None
            if not is_open and stream.random() > 0.6:
                continue
            if details["provider"] == "claude" and len(self.claude) < profile.claude_transcripts:
                spec = self.claude_spec(stream, details["cwd"], 120, recent=is_open)
                details["session_id"] = spec["session_id"]
            elif details["provider"] == "codex" and len(self.codex) < profile.codex_rollouts:
                spec = self.codex_spec(stream, details["cwd"], 120, recent=is_open)
                details["session_id"] = spec["thread_id"]
            if details["session_id"]:
                card["title"] = (spec["prompts"][0])[:60]
        if "long-project-path" in self.edge:
            # The long path is only an edge case once history is filed under it.
            (long,) = [p for p in self.projects if p["name"] == self.edge_cases["long-project-path"]["project"]]
            for _ in range(2):
                self.claude_spec(stream, long["checkout"], 120)
        places = [w["path"] for p in self.projects for w in p["worktrees"]]
        while len(self.claude) < profile.claude_transcripts:
            self.claude_spec(stream, places[stream.skewed(0, len(places) - 1, 1.4)], 120)
        while len(self.codex) < profile.codex_rollouts:
            self.codex_spec(stream, places[stream.skewed(0, len(places) - 1, 1.4)], 120)
        # Spawned Codex agents are threads of their own, never listed and
        # never a card's conversation.
        linked = {c["kind"]["session_id"] for c in chats}
        for spec in self.codex:
            if (stream.random() < profile.codex_subagent_fraction and spec["name"] is None
                    and spec["thread_id"] not in linked):
                parents = [s for s in self.codex if s is not spec and "parent" not in s]
                spec["parent"] = stream.choice(parents)["thread_id"]
                spec["nickname"] = stream.choice(["Reviewer", "Surveyor", "Tester"])
        self.plan_history_edge_cases(stream)
        open_codex = [c["kind"]["session_id"] for c in chats
                      if c["closed_at_ms"] is None and c["kind"]["provider"] == "codex" and c["kind"]["session_id"]]
        self.writer_locks = open_codex[: profile.writer_locks]

    def plan_history_edge_cases(self, stream):
        edge = self.edge
        # Damage goes on conversations no open card shows, recent enough to
        # be near the top of the resume picker.
        linked = {c["kind"]["session_id"] for c in self.cards if c["kind"]["type"] == "chat"}
        spare = [s for s in self.claude if s["session_id"] not in linked]
        spare.sort(key=lambda s: -s["started_at"])
        for case in ("malformed-record", "truncated-record", "unfinished-line", "renamed-file", "sidechain"):
            if case in edge and spare:
                spec = spare.pop(0)
                spec["edge"] = case
                if len(spec["prompts"]) < 3:
                    spec["prompts"] += [stream.choice(content.FOLLOW_UPS) for _ in range(3 - len(spec["prompts"]))]
                if case == "renamed-file":
                    spec["file_id"] = stream.uuid4()
                self.edge_cases[case] = {"session_id": spec["session_id"], "file_id": spec["file_id"],
                                         "cwd": spec["cwd"]}
        if "forked" in edge:
            forks = []
            for _ in range(max(1, len(self.claude) // 300)):
                if len(spare) < 2:
                    break
                origin, fork = spare.pop(0), spare.pop(0)
                others = [p for p in self.projects if not origin["cwd"].startswith(p["checkout"])]
                fork["cwd"] = stream.choice(others or self.projects)["checkout"]
                fork["branch"] = "main"
                fork["edge"] = "forked"
                fork["origin"] = origin["session_id"]
                fork["started_at"] = max(fork["started_at"], origin["started_at"] + 3600)
                forks.append({"session_id": fork["session_id"], "origin": origin["session_id"],
                              "cwd": fork["cwd"], "origin_cwd": origin["cwd"]})
            self.edge_cases["forked"] = forks
        if "large-record" in edge and spare:
            spec = spare.pop(0)
            spec["edge"] = "large-record"
            spec["large_mib"] = self.profile.large_record_mib
            self.edge_cases["large-record"] = {"session_id": spec["session_id"], "cwd": spec["cwd"],
                                               "mib": spec["large_mib"]}
        if "large-transcripts" in edge:
            large = []
            for _ in range(2):
                if not spare:
                    break
                spec = spare.pop(0)
                spec["edge"] = "large-transcripts"
                spec["pad_mib"] = self.profile.large_transcript_mib
                large.append({"session_id": spec["session_id"], "cwd": spec["cwd"]})
            self.edge_cases["large-transcripts"] = large
        if "oversized-transcript" in edge and spare:
            spec = spare.pop(0)
            spec["pad_mib"] = self.profile.oversized_mib
            spec["edge"] = "oversized-transcript"
            self.edge_cases["oversized-transcript"] = {"session_id": spec["session_id"], "cwd": spec["cwd"]}
        listed = [s for s in self.codex if "parent" not in s]
        listed.sort(key=lambda s: -s["started_at"])
        if "codex-interrupted-turn" in edge and listed:
            spec = listed.pop(0)
            spec["endings"][len(spec["prompts"]) - 1] = "interrupted"
            self.edge_cases["codex-interrupted-turn"] = {"thread_id": spec["thread_id"]}
        if "codex-usage-limit" in edge and listed:
            spec = listed.pop(0)
            spec["endings"][len(spec["prompts"]) - 1] = "usage-limit"
            self.edge_cases["codex-usage-limit"] = {"thread_id": spec["thread_id"]}
        if "codex-archived" in edge:
            linked_codex = linked
            archived = [s for s in listed if s["thread_id"] not in linked_codex][-max(1, len(self.codex) // 50):]
            for spec in archived:
                spec["archived"] = True
            self.edge_cases["codex-archived"] = [s["thread_id"] for s in archived]

    def edge_detail(self, spec, details):
        """What writing an edge case's conversation chose, for tests to find."""
        self.edge_cases[spec["edge"]].update(details)

    # Windows.

    def plan_windows(self):
        stream = self.streams("windows")
        open_cards = [c for c in self.cards if c["closed_at_ms"] is None]
        panes = [c for c in open_cards if c["kind"]["type"] not in UTILITY]
        scoped = [c for c in self.cards if c["scope"]]
        closed_scoped = [c for c in scoped if c["closed_at_ms"] is not None]
        card_scopes = [c["id"] for c in panes] + [c["id"] for c in stream.sample(
            closed_scoped, min(len(closed_scoped), max(0, len(self.cards) // 9)))]
        channel_with_panes = [c for c in self.channels if c["cards"]]
        live = [w for p in self.projects for w in p["worktrees"] if not w["removed"]]
        focus = {}
        for worktree in live:
            here = [c for c in panes if c["scope"] == worktree["path"]] or \
                   [c for c in scoped if c["scope"] == worktree["path"]]
            if here:
                focus[worktree["path"]] = here[0]["id"]
        chats = [c for c in panes if c["kind"]["type"] == "chat"]
        unread = {card: (entry["project"], entry["worktree"]) for entry in self.unread for card in entry["cards"]}
        for card in stream.sample(chats, min(len(chats), 3)):
            unread.setdefault(card["id"], None)
        primary = {
            "slot": "primary",
            "workspace_id": stream.uuid4(),
            "cards": [c["id"] for c in self.cards],
            "active_channel": channel_with_panes[0]["id"] if channel_with_panes else None,
            "active_worktree": live[0]["path"] if live else None,
            "focused": channel_with_panes[0]["focused"] if channel_with_panes else None,
            "card_scopes": card_scopes,
            "worktree_focus": focus,
            "unread": [{"card": card, "owner": owner} for card, owner in unread.items()],
            "drafts": {c["id"]: "Also check the retry path when the token" for c in chats[:2]},
            "pending_chat_admissions": [c["id"] for c in chats[: max(0, min(len(chats), len(chats) // 3))]],
            "user_titles": {c["id"]: f"Pinned: {c['title'][:30]}" for c in stream.sample(panes, min(len(panes), 6))},
            "collapsed_projects": [p["id"] for p in self.projects[len(self.projects) // 2:][::3]],
            "worktree_offsets": {w["path"]: round(stream.uniform(0, 1200), 1) for w in live},
        }
        windows = [primary]
        if self.profile.second_window_cards:
            closed = [c["id"] for c in self.cards if c["closed_at_ms"] is not None]
            keep = max(0, self.profile.second_window_cards - len(open_cards))
            subset = set(stream.sample(closed, min(len(closed), keep))) | {c["id"] for c in open_cards}
            # The name the app gives the second window it opens.
            second = dict(primary, slot="window-2", workspace_id=stream.uuid4(),
                          cards=[c["id"] for c in self.cards if c["id"] in subset],
                          active_channel=channel_with_panes[-1]["id"] if channel_with_panes else None,
                          focused=channel_with_panes[-1]["focused"] if channel_with_panes else None,
                          card_scopes=[card for card in card_scopes if card in subset],
                          drafts={}, pending_chat_admissions=[], unread=[], user_titles={})
            windows.append(second)
        self.windows = windows

    def plan(self):
        self.plan_projects()
        self.plan_environments()
        self.plan_views_and_cards()
        self.plan_history()
        self.plan_windows()
        return self
