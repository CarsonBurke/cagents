"""Named workspace shapes.

A profile is a set of knobs. Add a knob here, give it a default in `Profile`,
read it where the workspace or history is built, and cover it in
tests/fixture_workspace.py. `--scale` multiplies the counts marked "scaled"
and leaves the structure (projects, channels, edge cases) alone.
"""

import dataclasses

# Opt-in damage and unusual shapes, one name each. A profile lists the ones it
# includes; --edge-case adds more and --no-edge-cases drops them all. Each is
# written exactly once, into a transcript the plan names, so a test can find it.
EDGE_CASES = {
    "malformed-record": "A Claude record mid-file that is not JSON.",
    "unfinished-line": "A Claude transcript whose last line has no newline and is cut short.",
    "truncated-record": "A Claude record cut short mid-file, followed by intact records.",
    "renamed-file": "A Claude transcript whose file name is not its sessionId.",
    "sidechain": "A Claude transcript that also carries isSidechain records.",
    "forked": "A Claude fork: its origin's records under the origin sessionId and cwd first, "
    "filed under another project's folder.",
    "long-project-path": "A project path over 200 characters, so its Claude folder is a prefix plus a hash.",
    "removed-worktree": "History, a closed pane and a shared view for a worktree deleted from disk.",
    "codex-interrupted-turn": "A Codex turn ended by turn_aborted (reason interrupted).",
    "codex-usage-limit": "A Codex turn whose task_complete carries a usage_limit_exceeded error.",
    "codex-archived": "Codex rollouts moved to archived_sessions/.",
    "oversized-transcript": "A Claude transcript over 256 MiB, which the reader refuses. Writes about 260 MiB, so no "
    "profile and not 'all' include it; name it on its own.",
}

# Edge cases a profile never gets from "all"; each must be named on its own.
EXPLICIT_ONLY = {"oversized-transcript"}


@dataclasses.dataclass(frozen=True)
class Profile:
    name: str
    description: str
    # Projects and worktrees.
    projects: int
    live_worktrees: int  # Across all projects; checked out on disk.
    removed_worktrees: int  # Created, used, then deleted from disk.
    historical_worktree_scopes: int  # Scaled. Shared views for worktrees long gone.
    # Channels and the daemon's shared views.
    channels: int
    nonempty_views: int
    pane_mix: dict  # Open, viewed panes by kind.
    retired_views: int
    retired_max: int
    view_edits: int  # Scaled. Recorded view operations (shared_view_edits rows).
    view_edit_outliers: tuple
    # Window cards.
    cards: dict  # Scaled. Every card the primary window ever opened, by kind.
    open_fraction: float
    second_window_cards: int  # Scaled. 0 for one window.
    # History.
    claude_transcripts: int  # Scaled.
    codex_rollouts: int  # Scaled.
    turns: tuple  # (fewest, most) user turns in a conversation.
    long_turn_fraction: float  # Turns with long fenced code and tool output.
    subagent_fraction: float
    codex_subagent_fraction: float
    titled_fraction: float
    noise_fraction: float  # Conversations that open with harness noise.
    edge_cases: frozenset
    writer_locks: int = 0  # Codex threads whose writer lock `hold-writer-locks` takes.
    # The sidebar's resting states.
    avatars: int = 0  # Projects with a GitHub remote and a cached avatar.
    dirty_worktrees: int = 0  # Live worktrees with uncommitted changes.
    unread: tuple = ()  # Open chats with unread agent updates, per channel.
    oversized_mib: int = 260


EVERY_EDGE_CASE = frozenset(EDGE_CASES) - EXPLICIT_ONLY

# The kinds a window can hold, in the proportions of a long-lived install.
REALISTIC_CARDS = {
    "chat": 895,
    "terminal": 607,
    "repository": 149,
    "browser": 40,
    "accounts": 29,
    "settings": 24,
    "automations": 13,
    "pull_requests": 6,
    "work": 6,
}

PROFILES = {
    "small": Profile(
        name="small",
        description="Fast and clean, for tests: a few projects, channels and conversations.",
        projects=3,
        live_worktrees=3,
        removed_worktrees=1,
        historical_worktree_scopes=12,
        channels=6,
        nonempty_views=5,
        pane_mix={"chat": 5, "terminal": 3, "repository": 1},
        retired_views=4,
        retired_max=5,
        view_edits=40,
        view_edit_outliers=(12,),
        cards={"chat": 14, "terminal": 9, "repository": 3, "browser": 2, "settings": 1, "accounts": 1,
               "automations": 1, "pull_requests": 1, "work": 1},
        open_fraction=0.4,
        second_window_cards=10,
        claude_transcripts=24,
        codex_rollouts=8,
        turns=(1, 5),
        long_turn_fraction=0.3,
        subagent_fraction=0.2,
        codex_subagent_fraction=0.2,
        titled_fraction=0.5,
        noise_fraction=0.3,
        edge_cases=frozenset(),
    ),
    "realistic": Profile(
        name="realistic",
        description="The size and shape of a long-lived installation, with one of each kind of damage.",
        projects=18,
        live_worktrees=40,
        removed_worktrees=12,
        historical_worktree_scopes=1780,
        channels=76,
        nonempty_views=44,
        pane_mix={"chat": 52, "terminal": 27, "repository": 4},
        retired_views=89,
        retired_max=63,
        view_edits=45970,
        view_edit_outliers=(36608, 5891, 1320),
        cards=REALISTIC_CARDS,
        open_fraction=0.05,
        second_window_cards=1709,
        claude_transcripts=2400,
        codex_rollouts=700,
        turns=(1, 14),
        long_turn_fraction=0.25,
        subagent_fraction=0.12,
        codex_subagent_fraction=0.2,
        titled_fraction=0.45,
        noise_fraction=0.18,
        edge_cases=EVERY_EDGE_CASE,
        writer_locks=2,
    ),
    "huge": Profile(
        name="huge",
        description="Stress: thousands of conversations and every edge case.",
        projects=24,
        live_worktrees=60,
        removed_worktrees=30,
        historical_worktree_scopes=3000,
        channels=150,
        nonempty_views=90,
        pane_mix={"chat": 110, "terminal": 55, "repository": 8},
        retired_views=160,
        retired_max=120,
        view_edits=60000,
        view_edit_outliers=(40000, 8000, 2000),
        cards={kind: count * 2 for kind, count in REALISTIC_CARDS.items()},
        open_fraction=0.05,
        second_window_cards=3000,
        claude_transcripts=10000,
        codex_rollouts=2000,
        turns=(1, 24),
        long_turn_fraction=0.3,
        subagent_fraction=0.15,
        codex_subagent_fraction=0.2,
        titled_fraction=0.45,
        noise_fraction=0.18,
        edge_cases=EVERY_EDGE_CASE,
        writer_locks=4,
    ),
    "sidebar": Profile(
        name="sidebar",
        description="The sidebar's resting states: avatars and initials, dirty worktrees, unread counts.",
        projects=10,
        live_worktrees=18,
        removed_worktrees=2,
        historical_worktree_scopes=20,
        channels=24,
        nonempty_views=12,
        pane_mix={"chat": 12, "terminal": 6, "repository": 2},
        retired_views=4,
        retired_max=4,
        view_edits=0,
        view_edit_outliers=(),
        cards={"chat": 60, "terminal": 30, "repository": 6, "browser": 2, "settings": 1, "accounts": 1,
               "automations": 1, "pull_requests": 1, "work": 1},
        open_fraction=0.5,
        second_window_cards=0,
        claude_transcripts=80,
        codex_rollouts=20,
        turns=(1, 6),
        long_turn_fraction=0.2,
        subagent_fraction=0.1,
        codex_subagent_fraction=0.1,
        titled_fraction=0.6,
        noise_fraction=0.1,
        edge_cases=frozenset(),
        avatars=5,
        dirty_worktrees=6,
        unread=(3, 12, 120),
    ),
}

SCALED = ("historical_worktree_scopes", "view_edits", "second_window_cards", "claude_transcripts", "codex_rollouts")


def resolve(name, scale=1.0, add=(), drop_all=False):
    """The profile to generate: named, scaled, with edge cases adjusted."""
    try:
        profile = PROFILES[name]
    except KeyError:
        raise SystemExit(f"Unknown profile {name!r}. Choose one of: {', '.join(PROFILES)}.") from None
    if scale <= 0:
        raise SystemExit("--scale must be positive.")
    changes = {field: max(0, round(getattr(profile, field) * scale)) for field in SCALED}
    changes["cards"] = {kind: max(1, round(count * scale)) for kind, count in profile.cards.items()}
    changes["view_edit_outliers"] = tuple(round(count * scale) for count in profile.view_edit_outliers)
    edge = set() if drop_all else set(profile.edge_cases)
    for case in add:
        if case == "all":
            edge |= EVERY_EDGE_CASE
        elif case in EDGE_CASES:
            edge.add(case)
        else:
            raise SystemExit(f"Unknown edge case {case!r}. Choose one of: all, {', '.join(EDGE_CASES)}.")
    changes["edge_cases"] = frozenset(edge)
    return dataclasses.replace(profile, **changes)
