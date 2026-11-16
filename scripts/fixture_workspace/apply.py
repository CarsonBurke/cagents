"""Loads a generated plan into the app's stores through the development
executables' own writers (see docs/fixture-workspace.md).

The daemon writes the shared workspace model first; its identity tells each
window which channels the registry already holds.
"""

import json
import os
import re
import shutil
import subprocess
import tempfile

from .generate import MARKER


def load_plan(root):
    path = os.path.join(os.path.realpath(root), MARKER)
    try:
        with open(path) as file:
            return path, json.load(file)
    except FileNotFoundError:
        raise SystemExit(f"No {MARKER} in {root}. Run `scripts/fixture-workspace generate` there first.") from None


def environment(root, instance, runtime=None):
    """Where the stores go. Without --instance, everything stays inside the
    fixture root, with the private runtime directory `runtime`, so no running
    daemon is involved. With --instance, the stores are that development
    instance's."""
    root = os.path.realpath(root)
    env = dict(os.environ)
    if instance is None:
        env.update(HOME=os.path.join(root, "home"), XDG_DATA_HOME=os.path.join(root, "data"),
                   XDG_CONFIG_HOME=os.path.join(root, "config"), XDG_CACHE_HOME=os.path.join(root, "cache"),
                   XDG_STATE_HOME=os.path.join(root, "state"), XDG_RUNTIME_DIR=runtime)
        env.pop("CAGENTS_DEVELOPMENT_INSTANCE", None)
    else:
        # As the development executables and scripts/run-development accept it.
        if not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9-]{0,23}", instance):
            raise SystemExit("--instance needs 1-24 ASCII letters, digits or hyphens, starting with a letter or "
                             "digit. The unnamed development instance is the login daemon's and is never seeded.")
        env["CAGENTS_DEVELOPMENT_INSTANCE"] = instance
    return env


def run(command, env, what):
    result = subprocess.run(command, env=env, capture_output=True, text=True)
    if result.returncode != 0:
        raise SystemExit(f"{what} failed (exit {result.returncode}):\n{result.stderr.strip() or result.stdout.strip()}")
    return result.stdout


def apply(root, bin_dir, instance=None, log=print):
    plan = load_plan(root)
    if plan[1]["root"] != os.path.realpath(root):
        raise SystemExit(f"{root} holds a plan generated at {plan[1]['root']}, whose repositories and history its "
                         "paths name. Generate it again where it is to be applied.")
    if instance is not None:
        return load(plan, bin_dir, instance, None, log)
    # The seed's leases live here only while it runs: a fixture copied to
    # another place must not carry a stale daemon lock with it.
    runtime = tempfile.mkdtemp(prefix=".rt-", dir=os.path.realpath(root))
    try:
        return load(plan, bin_dir, None, runtime, log)
    finally:
        shutil.rmtree(runtime, ignore_errors=True)


def load(loaded, bin_dir, instance, runtime, log):
    plan_path, plan = loaded
    env = environment(plan["root"], instance, runtime)
    daemon = os.path.join(bin_dir, "cagentsd")
    window = os.path.join(bin_dir, "cagents-ui")
    for executable in (daemon, window):
        if not os.access(executable, os.X_OK):
            raise SystemExit(f"{executable} is missing. Pass --bin-dir the directory holding a development build "
                             "of cagentsd and cagents-ui: the dev-runtime directory scripts/run-development builds "
                             "into, or a native-evidence side's target-SIDE/dev-runtime.")
    output = run([daemon, "--development-seed-workspace", plan_path], env, "Seeding the daemon's workspace model")
    seeded = json.loads(output.strip().splitlines()[-1])
    log(f"Daemon: {seeded['projects']} projects, {seeded['channels']} channels, {seeded['views']} shared views "
        f"({seeded['panes']} panes, {seeded['edits']} recorded edits) in {seeded['seconds']:.1f}s "
        f"-> {seeded['path']}")
    for spec in plan["windows"]:
        output = run([window, "--development-seed-workspace", plan_path, "--window", spec["slot"],
                      "--workspace-model-identity", seeded["identity"]], env,
                     f"Seeding the {spec['slot']} window's session")
        result = json.loads(output.strip().splitlines()[-1])
        log(f"Window {spec['slot']}: {result['cards']} cards ({result['open']} open), {result['channels']} channels "
            f"in {result['seconds']:.1f}s -> {result['path']}")
    return seeded
