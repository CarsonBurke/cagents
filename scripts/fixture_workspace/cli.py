"""Command line for scripts/fixture-workspace. See docs/fixture-workspace.md."""

import argparse
import signal
import sys

from . import apply, codex, codex_check, profiles
from .determinism import EPOCH
from .generate import generate


def parser():
    top = argparse.ArgumentParser(
        prog="scripts/fixture-workspace",
        description="Generate a realistic, reproducible cagents workspace into a fixture directory.")
    commands = top.add_subparsers(dest="command", required=True)

    make = commands.add_parser("generate", help="Write repositories, conversation history and plan.json.")
    make.add_argument("--profile", default="small", help=f"One of: {', '.join(profiles.PROFILES)}.")
    make.add_argument("--seed", type=int, default=1, help="The same profile, seed and root write the same bytes.")
    make.add_argument("--root", required=True, help="An empty directory, or one generated before with --replace.")
    make.add_argument("--scale", type=float, default=1.0, help="Multiply the profile's counts.")
    make.add_argument("--edge-case", action="append", default=[], metavar="NAME",
                      help=f"Add an edge case, or 'all'. One of: {', '.join(profiles.EDGE_CASES)}.")
    make.add_argument("--no-edge-cases", action="store_true", help="Drop the profile's edge cases.")
    make.add_argument("--epoch", type=int, default=EPOCH, help="Unix seconds every timestamp counts back from.")
    make.add_argument("--replace", action="store_true", help="Delete a workspace generated at --root before.")

    load = commands.add_parser("apply", help="Write plan.json into the daemon's and windows' stores.")
    load.add_argument("--root", required=True)
    load.add_argument("--bin-dir", required=True, help="Holds development builds of cagentsd and cagents-ui.")
    load.add_argument("--instance", help="Seed this development instance's own storage instead of the root's.")

    hold = commands.add_parser("hold-writer-locks",
                               help="Hold the Codex writer locks plan.json names, as a live codex would, until "
                                    "interrupted.")
    hold.add_argument("--root", required=True)
    hold.add_argument("--thread", action="append", default=[], help="Lock this thread instead. Repeatable.")

    probe = commands.add_parser("check-codex",
                                help="Read every rollout back through a real codex app-server (skipped without "
                                     "codex on PATH).")
    probe.add_argument("--root", required=True)

    lists = commands.add_parser("edge-cases", help="List the edge cases and the profiles that include them.")
    del lists
    return top


def main(argv=None):
    arguments = parser().parse_args(argv)
    if arguments.command == "generate":
        profile = profiles.resolve(arguments.profile, arguments.scale, arguments.edge_case, arguments.no_edge_cases)
        generate(profile, arguments.seed, arguments.root, arguments.epoch, arguments.replace)
    elif arguments.command == "apply":
        apply.apply(arguments.root, arguments.bin_dir, arguments.instance)
    elif arguments.command == "hold-writer-locks":
        _, plan = apply.load_plan(arguments.root)
        threads = arguments.thread or plan["history"]["writer_locks"]
        if not threads:
            raise SystemExit("plan.json names no writer locks; pass --thread ID.")
        held = codex.hold_writer_locks(f"{plan['home']}/.codex", threads)
        print(f"Holding {len(held)} Codex writer locks; interrupt to release.", flush=True)
        signal.signal(signal.SIGTERM, lambda *_: sys.exit(0))
        try:
            signal.pause()
        except KeyboardInterrupt:
            pass
    elif arguments.command == "check-codex":
        _, plan = apply.load_plan(arguments.root)
        mismatched = codex_check.check(plan["home"], plan["projects"][0]["checkout"])
        if mismatched:
            raise SystemExit(1)
    elif arguments.command == "edge-cases":
        for name, description in profiles.EDGE_CASES.items():
            including = [p.name for p in profiles.PROFILES.values() if name in p.edge_cases]
            print(f"{name}: {description} Profiles: {', '.join(including) or 'none (opt in)'}.")
