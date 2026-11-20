#!/usr/bin/python3
"""Saved Claude terminals for reboot recovery evidence.

Usage:
  cleared-terminal-recovery.py history ROOT
  cleared-terminal-recovery.py inspect ROOT
  cleared-terminal-recovery.py reboot ROOT

`history` writes two invented saved conversations, Cleared and Ordinary,
under ROOT/home/.claude for ROOT/project. Return each to a terminal from its
native chat; in the Cleared one, run `/clear` and send nothing.

`inspect` prints every saved terminal pane's launch and its recovery hint,
whose version says whether the terminal was last seen in an exact
conversation (1) or only recognized as Claude (2).

`reboot` makes the stopped fixture look as it does after the machine
restarts: every record in its stores of the current kernel boot names an
earlier one instead, and nothing else changes. The terminals' processes
must already be gone, as `native-evidence stop` leaves them. A receipt goes to
ROOT/cleared-terminal-reboot.json; a fixture is rebooted once.

No provider, daemon or network service is used.
"""
import argparse
from datetime import datetime, timedelta, timezone
import json
import os
from pathlib import Path
import pwd
import sqlite3
import uuid

CONVERSATIONS = {
    "Cleared": "a1b2c3d4-1111-4111-8111-000000003390",
    "Ordinary": "a1b2c3d4-2222-4111-8111-000000003390",
}
PROMPTS = {
    "Cleared": "Cleared terminal conversation: summarize the failing release check.",
    "Ordinary": "Ordinary terminal conversation: list the open review comments.",
}


def claude_home(root):
    home = root / "home" / ".claude"
    account = Path(pwd.getpwuid(os.getuid()).pw_dir) / ".claude"
    if home.resolve() == account.resolve():
        raise ValueError("refusing the account's real Claude home")
    return home


def project_directory(root):
    project = (root / "project").resolve(strict=True)
    encoded = "".join(c if c.isascii() and c.isalnum() else "-" for c in str(project))
    return project, claude_home(root) / "projects" / encoded


def history(root):
    project, directory = project_directory(root)
    directory.mkdir(parents=True, exist_ok=True)
    base = datetime.now(timezone.utc) - timedelta(minutes=10)
    written = {}
    for offset, (name, thread) in enumerate(CONVERSATIONS.items()):
        path = directory / f"{thread}.jsonl"
        parent = None
        rows = []
        for step, (role, text) in enumerate([
            ("user", PROMPTS[name]),
            ("assistant", f"{name} conversation reply: the saved transcript is readable offline."),
        ]):
            message = str(uuid.uuid4())
            rows.append({
                "type": role, "uuid": message, "parentUuid": parent,
                "sessionId": thread, "cwd": str(project),
                "timestamp": (base + timedelta(minutes=offset, seconds=step))
                .isoformat().replace("+00:00", "Z"),
                "message": {"id": message, "role": role, "model": "haiku",
                            "content": [{"type": "text", "text": text}]},
            })
            parent = message
        # A rerun cannot overwrite a transcript a provider may have loaded.
        with path.open("x") as output:
            for row in rows:
                output.write(json.dumps(row, separators=(",", ":")) + "\n")
        os.chmod(path, 0o600)
        written[name] = {"thread_id": thread, "history": str(path)}
    return {"working_directory": str(project), "conversations": written}


def database(root):
    found = sorted((root / "data").rglob("workspace-model.sqlite3"))
    if len(found) != 1:
        raise ValueError(f"expected one workspace model under {root / 'data'}, found {len(found)}")
    return found[0]


def terminal_descriptors(connection):
    """Each saved terminal pane's descriptor, as (pane, JSON)."""
    return connection.execute(
        "SELECT id, descriptor FROM panes WHERE kind = 'terminal' ORDER BY position").fetchall()


def sessions(value):
    """Terminal sessions inside one stored JSON value."""
    found = []

    def walk(node):
        if isinstance(node, dict):
            if "launch" in node and "continuity" in node and "id" in node:
                found.append(node)
            for child in node.values():
                walk(child)
        elif isinstance(node, list):
            for child in node:
                walk(child)
    walk(value)
    return found


def summary(session):
    hint = session.get("resume_hint")
    version = None
    if hint:
        try:
            version = json.loads(hint).get("version")
        except (ValueError, AttributeError):
            version = "unreadable"
    launch = session["launch"]
    return {
        "terminal": session["id"],
        "launch": [launch["program"], *launch["arguments"]],
        "continuity": next(iter(session["continuity"])) if isinstance(
            session["continuity"], dict) else session["continuity"],
        "hint_version": version,
        "hinted_launch": json.loads(hint)["recipe"]["arguments"] if version else None,
    }


def inspect(root):
    connection = sqlite3.connect(f"file:{database(root)}?mode=ro", uri=True)
    result = []
    for pane, value in terminal_descriptors(connection):
        for session in sessions(json.loads(value)):
            result.append({"pane": pane, **summary(session)})
    return result


def current_boot():
    return Path("/proc/sys/kernel/random/boot_id").read_text().strip()


def reboot(root):
    receipt = root / "cleared-terminal-reboot.json"
    if receipt.exists():
        raise ValueError(f"this fixture was already rebooted; see {receipt}")
    boot = current_boot()
    previous = f"fixture-boot-before-{uuid.uuid4()}"
    stores = sorted((root / "data").rglob("*.sqlite3"))
    if database(root) not in stores:
        raise ValueError("the workspace model is missing")
    connections = []
    try:
        for path in stores:
            connection = sqlite3.connect(path, timeout=0, isolation_level=None)
            connections.append((path, connection))
            # Holding every write lock proves no daemon or window is running,
            # as after a restart nothing from this boot is.
            connection.execute("BEGIN EXCLUSIVE")
        changed = []
        for path, connection in connections:
            for (table,) in connection.execute(
                    "SELECT name FROM sqlite_master WHERE type = 'table'").fetchall():
                columns = [row[1] for row in connection.execute(
                    f'PRAGMA table_info("{table}")') if row[2].upper() == "TEXT"]
                for column in columns:
                    count = connection.execute(
                        f'UPDATE "{table}" SET "{column}" = replace("{column}", ?, ?) '
                        f'WHERE instr("{column}", ?) > 0', (boot, previous, boot)).rowcount
                    if count:
                        changed.append({"store": path.name, "table": table,
                                        "column": column, "rows": count})
        if not any(entry["table"] == "panes" for entry in changed):
            raise ValueError("no saved pane recorded this boot; nothing to reboot")
        for _, connection in connections:
            connection.execute("COMMIT")
    except BaseException:
        for _, connection in connections:
            if connection.in_transaction:
                connection.execute("ROLLBACK")
        raise
    report = {"boot": boot, "recorded_boot": previous, "changed": changed}
    with receipt.open("x") as output:
        json.dump(report, output, indent=2)
    return report


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__,
                                     formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("command", choices=["history", "inspect", "reboot"])
    parser.add_argument("root", type=Path)
    args = parser.parse_args()
    try:
        root = args.root.resolve(strict=True)
        print(json.dumps({"history": history, "inspect": inspect,
                          "reboot": reboot}[args.command](root), indent=2))
    except (OSError, ValueError, sqlite3.Error) as error:
        parser.exit(1, f"cleared-terminal-recovery: {error}\n")
