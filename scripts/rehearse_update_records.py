#!/usr/bin/python3
"""Read-only rehearsal snapshots and deterministic before/after comparisons."""
from __future__ import annotations

import argparse
from contextlib import closing
import hashlib
import json
import os
from pathlib import Path
import re
import sqlite3
import stat
import sys
from typing import Any


COLLECTIONS = ("terminals", "sessions", "channels", "projects", "agents")
EVIDENCE_LIMIT = 8 * 1024 * 1024


def read_json(path: Path) -> Any:
    return json.loads(path.read_text())


def dump(value: Any) -> str:
    return json.dumps(value, indent=2, sort_keys=True, ensure_ascii=False) + "\n"


def private_evidence(path: Path) -> bytes:
    descriptor = os.open(path, os.O_RDONLY | os.O_NOFOLLOW)
    with os.fdopen(descriptor, "rb") as stream:
        metadata = os.fstat(stream.fileno())
        if (not stat.S_ISREG(metadata.st_mode) or metadata.st_uid != os.geteuid()
                or metadata.st_mode & 0o077 or metadata.st_size > EVIDENCE_LIMIT):
            raise ValueError(f"unsafe or oversized recovery evidence: {path.name}")
        raw = stream.read(EVIDENCE_LIMIT + 1)
        if len(raw) > EVIDENCE_LIMIT:
            raise ValueError(f"recovery evidence exceeds size limit: {path.name}")
        return raw


def cli_rows(out: Path, name: str, errors: list[str]) -> list[dict]:
    path = out / f"{name}.json"
    try:
        value = read_json(path)
        if not isinstance(value, dict) or value.get("ok") is False:
            raise ValueError(f"command refused: {value}")
        data = value.get("data", value)
        rows = data.get(name)
        if not isinstance(rows, list):
            raise ValueError(f"missing {name} array")
        if not all(isinstance(row, dict) for row in rows):
            raise ValueError("inventory contains non-object rows")
        return rows
    except (OSError, ValueError, AttributeError) as error:
        detail = ""
        stderr = out / f"{name}.stderr"
        if stderr.is_file():
            detail = "; stderr: " + stderr.read_text(errors="replace").strip()
        errors.append(f"{path.name}: {error}{detail}")
        return []


def identity(row: dict, name: str) -> str:
    if name == "agents":
        agent = row.get("agent")
        return str(agent.get("id", "")) if isinstance(agent, dict) else str(row.get("id", ""))
    if name == "sessions":
        lineage = row.get("lineage") or row.get("id", "")
        return json.dumps(lineage, sort_keys=True) if isinstance(lineage, dict) else str(lineage)
    if name == "projects":
        return str(row.get("repository") or row.get("id", ""))
    session = row.get("session")
    return str(row.get("id") or (session.get("id", "") if isinstance(session, dict) else ""))


def index_rows(rows: list[dict], name: str) -> dict[str, dict]:
    return {identity(row, name): row for row in rows}


def decoded_row(row: sqlite3.Row) -> dict:
    result = dict(row)
    for key, value in list(result.items()):
        if key.endswith("_json") and isinstance(value, str):
            try:
                result[key[:-5]] = json.loads(value)
                del result[key]
            except ValueError:
                pass
        elif isinstance(value, bytes):
            result[key] = {"hex": value.hex()}
    return result


def stores(home: Path, errors: list[str]) -> dict:
    base = home / ".local/share/cagents"
    paths = set(base.glob("windows/*/sessions.sqlite3"))
    if (base / "sessions.sqlite3").is_file():
        paths.add(base / "sessions.sqlite3")
    result = {}
    for path in sorted(paths):
        key = str(path.relative_to(base))
        try:
            # mode=ro includes the live WAL. immutable=1 would silently miss it.
            with closing(sqlite3.connect(path.as_uri() + "?mode=ro", uri=True, timeout=5)) as db:
                db.row_factory = sqlite3.Row
                db.execute("PRAGMA query_only=ON")
                db.execute("BEGIN")
                names = [row[0] for row in db.execute(
                    "SELECT name FROM sqlite_master WHERE type='table' ORDER BY name"
                )]
                selected = {}
                for name in ("cards", "workspaces", "projects", "terminal_recipes"):
                    if name in names:
                        selected[name] = sorted(
                            (decoded_row(row) for row in db.execute(f'SELECT * FROM "{name}"')),
                            key=lambda row: json.dumps(row, sort_keys=True),
                        )
                if "cards" not in names:
                    errors.append(f"{key}: no cards table; pane inventory unavailable")
                result[key] = {"schema": db.execute("PRAGMA user_version").fetchone()[0],
                               "tables": names, "rows": selected}
        except sqlite3.Error as error:
            errors.append(f"{key}: {error}")
    if not result:
        errors.append("no sessions.sqlite3 stores found; pane inventory unavailable")
    return result


def processes(root: Path, errors: list[str] | None = None) -> list[dict]:
    result = []
    own = os.getpid()
    home_marker = f"HOME={root / 'home'}".encode()
    runtime_marker = f"XDG_RUNTIME_DIR={root / 'rt'}".encode()
    for proc in Path("/proc").glob("[0-9]*"):
        try:
            env = (proc / "environ").read_bytes().split(b"\0")
            if int(proc.name) == own or not (home_marker in env or runtime_marker in env):
                continue
            stat = (proc / "stat").read_text().rsplit(")", 1)[1].split()
            terminal = next((item.split(b"=", 1)[1].decode(errors="replace") for item in env
                             if item.startswith(b"CAGENTS_TERMINAL_ID=")), None)
            row = {"pid": int(proc.name), "ppid": int(stat[1]), "state": stat[0],
                   "start_ticks": int(stat[19]), "terminal_id": terminal,
                   "executable": os.readlink(proc / "exe"),
                   "argv": (proc / "cmdline").read_bytes().decode(errors="replace").rstrip("\0").split("\0")}
            confirmed = (proc / "stat").read_text().rsplit(")", 1)[1].split()
            if int(confirmed[19]) == row["start_ticks"]:
                result.append(row)
        except (OSError, ValueError, IndexError):
            continue
    by_identity = {(row["pid"], row["start_ticks"]): row for row in result}
    for path in sorted((root / "rt/cagents/holders").glob("*.json")):
        try:
            holder = read_json(path)
            for role in ("holder", "root"):
                process = holder[role]
                row = by_identity.get((process["pid"], process["start"]))
                if row is not None:
                    row.setdefault("holder_roles", []).append({"role": role, "terminal_id": holder["terminal"]})
                    if row.get("terminal_id") is None:
                        row["terminal_id"] = holder["terminal"]
        except (OSError, ValueError, KeyError, TypeError) as error:
            if errors is not None:
                errors.append(f"{path.relative_to(root)}: invalid holder registry: {error}")
    child_file = root / "child.pid"
    if child_file.exists():
        try:
            child_pid = int(child_file.read_text().strip())
            child = next((row for row in result if row["pid"] == child_pid), None)
            if child is None:
                raise ValueError(f"PID {child_pid} is absent from private processes")
            if not (len(child["argv"]) == 2 and Path(child["argv"][0]).name == "sleep"
                    and child["argv"][1] == "86400"):
                raise ValueError(f"PID {child_pid} no longer runs the seeded sleep 86400")
            child["role"] = "long-lived-child"
        except (OSError, ValueError) as error:
            if errors is not None:
                errors.append(f"child.pid: {error}")
    return sorted(result, key=lambda row: row["pid"])


def prepare_recovery(root: Path, out: Path) -> bool:
    """Copy this job's evidence verbatim; never choose a receipt by mtime."""
    state = root / "home/.local/share/cagents-install/.update"
    if not (state / "status.json").exists():
        return False
    status = read_json(state / "status.json")
    job = status["id"]
    if not isinstance(job, str) or not re.fullmatch(r"[0-9a-fA-F-]{36}", job):
        raise ValueError("invalid update transaction identity")
    paths = [state / f"{job}.{suffix}.json" for suffix in ("census", "recovery")]
    if not all(path.is_file() for path in paths):
        # An update refused before shutdown need not issue a recovery receipt.
        # It must still pass the ordinary exact before/after preservation checks.
        if status.get("phase") == "Complete":
            raise ValueError(f"completed update {job} has no paired census and readiness evidence")
        return False
    version = status.get("version")
    if not isinstance(version, str) or not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9.+_-]{0,127}", version):
        raise ValueError("invalid update target version")
    for suffix, name in (("census", "recovery-census"), ("recovery", "recovery-issued")):
        source = state / f"{job}.{suffix}.json"
        raw = private_evidence(source)
        destination = out / f"{name}.json"
        descriptor = os.open(destination, os.O_WRONLY | os.O_CREAT | os.O_TRUNC | os.O_NOFOLLOW, 0o600)
        with os.fdopen(descriptor, "wb") as stream:
            os.fchmod(stream.fileno(), 0o600)
            stream.write(raw)
    # A restored old current may predate the verifier. The staged target owns
    # the protocol bridge and receipt schema for this exact update job.
    print(root / "home/.local/share/cagents-install/versions" / version / "bin/cagents")
    return True


def recovery_evidence(out: Path, errors: list[str]) -> dict | None:
    census_file = out / "recovery-census.json"
    error_file = out / "recovery-error.txt"
    if error_file.exists():
        errors.append(f"recovery evidence: {error_file.read_text().strip()}")
        return None
    if not census_file.exists():
        return None
    try:
        raw = private_evidence(census_file)
        evidence = {"census": json.loads(raw), "digest": hashlib.sha256(raw).hexdigest(),
                    "issued": json.loads(private_evidence(out / "recovery-issued.json")),
                    "current": json.loads(private_evidence(out / "recovery-current.json"))}
        return evidence
    except (OSError, ValueError) as error:
        errors.append(f"recovery evidence: {error}")
        return None


def record(root: Path, out: Path, windows: Path | None) -> dict:
    errors: list[str] = []
    snapshot = {"schema": 1, "root": str(root), "errors": errors}
    for name in COLLECTIONS:
        rows = cli_rows(out, name, errors)
        indexed = index_rows(rows, name)
        if "" in indexed or len(indexed) != len(rows):
            errors.append(f"{name}: missing or duplicate stable identity")
        snapshot[name] = indexed
    try:
        snapshot["serving"] = read_json(out / "terminals.json").get("data", {}).get("proof")
    except (OSError, ValueError, AttributeError):
        snapshot["serving"] = None
    snapshot["recovery"] = recovery_evidence(out, errors)
    snapshot["stores"] = stores(root / "home", errors)
    snapshot["processes"] = processes(root, errors)
    snapshot["windows"] = {}
    if windows:
        try:
            observed = read_json(windows)
            if not isinstance(observed, dict):
                raise ValueError("expected object keyed by requested window name")
            if not observed:
                errors.append("no mapped windows observed")
            for name, payload in observed.items():
                if not isinstance(payload, dict) or not isinstance(payload.get("window"), dict):
                    errors.append(f"window {name}: missing observation: {payload}")
                    continue
                snapshot["windows"][name] = {**payload["window"], "painted": payload.get("painted")}
        except (OSError, ValueError) as error:
            errors.append(f"windows: {error}")
    else:
        errors.append("window observations unavailable")
    out.mkdir(parents=True, exist_ok=True)
    (out / "snapshot.json").write_text(dump(snapshot))
    return snapshot


def load_snapshot(path: Path) -> dict:
    if path.is_dir():
        path /= "snapshot.json"
    value = read_json(path)
    if not isinstance(value, dict) or value.get("schema") != 1:
        raise ValueError(f"{path}: unsupported or missing snapshot schema")
    for name in (*COLLECTIONS, "windows", "stores"):
        if not isinstance(value.get(name), dict):
            raise ValueError(f"{path}: missing {name} inventory")
    if not isinstance(value.get("processes"), list) or not isinstance(value.get("errors"), list):
        raise ValueError(f"{path}: missing processes/errors inventory")
    return value


def project(row: dict, name: str) -> dict:
    if name == "agents":
        agent = row.get("agent", {})
        spec = agent.get("spec", {})
        return {"state": row.get("state"), "name": agent.get("name"), "closed": agent.get("closed"),
                "placement": agent.get("placement", agent.get("channel")),
                "provider": spec.get("provider"), "surface": spec.get("surface")}
    fields = {
        "windows": ("app_id", "title", "size", "painted"),
        "terminals": ("state", "title", "program", "working_directory", "attached", "exit"),
        "sessions": ("id", "surface", "provider", "title", "channel", "state", "failure", "directory"),
        "channels": ("title", "name", "repository", "state", "closed"),
        "projects": ("name", "repository", "state", "closed"),
        "panes": ("workspace_id", "ordinal", "title", "kind_tag", "existence", "terminal_id", "session_id"),
    }[name]
    return {field: row[field] for field in fields if field in row}


def panes(snapshot: dict) -> dict:
    result = {}
    for database, store in snapshot["stores"].items():
        rows = store.get("rows", {})
        terminal_ids = {row["card_id"]: row["terminal_id"] for row in rows.get("terminal_recipes", [])}
        for card in rows.get("cards", []):
            card = dict(card)
            # Tombstones are retained in the raw snapshot, but do not represent
            # panes the user had open before the upgrade.
            if card.get("existence") != "open":
                continue
            card["terminal_id"] = terminal_ids.get(card["card_id"])
            content = card.get("content")
            if isinstance(content, dict) and card.get("kind_tag") == "chat":
                card["session_id"] = content.get("id")
            result[f"{database}:{card['card_id']}"] = card
    return result


def verified_pauses(before: dict, after: dict) -> tuple[set[str], set[str]]:
    """Receipts establish disposition; before records establish exact identity."""
    evidence = after.get("recovery")
    if not isinstance(evidence, dict):
        return set(), set()
    census, issued, current = (evidence.get(name, {}) for name in ("census", "issued", "current"))
    source = census.get("source")
    if (not isinstance(source, dict) or not source or source != before.get("serving")
            or any(not isinstance(source.get(field), str) or not source[field]
                   for field in ("host_session_id", "boot_id", "host_incarnation"))):
        return set(), set()
    for receipt in (issued, current):
        if (receipt.get("version") != 1 or census.get("version") != 1
                or receipt.get("transaction") != census.get("transaction")
                or receipt.get("census_digest") != evidence.get("digest")
                or receipt.get("checkpoint_before") != census.get("checkpoint_before")):
            return set(), set()
    outcome = current.get("outcome", {})
    if (issued.get("outcome") != outcome or outcome.get("state") != "usable"
            or not isinstance(outcome.get("serving"), dict)
            or outcome.get("serving") != after.get("serving")):
        return set(), set()
    chat_proofs = {card["content"].get("id"): card["content"].get("proof")
                   for card in panes(before).values()
                   if card.get("kind_tag") == "chat" and isinstance(card.get("content"), dict)}
    chat_ids = {row.get("id") for row in before["sessions"].values()}
    expected_chats = {row["id"] for row in census.get("chats", [])
                      if row.get("id") in chat_ids and row.get("proof") == source
                      and chat_proofs.get(row.get("id")) == source}
    expected_terminals = {row["id"] for row in census.get("terminals", [])
                          if row.get("id") in before["terminals"]}
    chats, terminals = outcome.get("paused_chats", []), outcome.get("paused_terminals", [])
    if (not isinstance(chats, list) or not isinstance(terminals, list)
            or not all(isinstance(item, str) for item in chats + terminals)
            or len(set(chats)) != len(chats) or len(set(terminals)) != len(terminals)
            or not set(chats) <= expected_chats or not set(terminals) <= expected_terminals):
        return set(), set()
    return set(chats), set(terminals)


def compare(before: dict, after: dict) -> list[dict]:
    changes = []
    paused_chats, paused_terminals = verified_pauses(before, after)
    for side, snapshot in (("before", before), ("after", after)):
        for error in snapshot["errors"]:
            changes.append({"kind": "evidence", "id": side, "change": str(error), "regression": True})
    for name in (*COLLECTIONS, "windows", "panes"):
        left = panes(before) if name == "panes" else before[name]
        right = panes(after) if name == "panes" else after[name]
        for key, original in sorted(left.items()):
            session = original.get("agent", {}).get("session") if name == "agents" else None
            chat = session.get("Chat") if isinstance(session, dict) else None
            paused_agent = (isinstance(chat, dict) and chat.get("id") in paused_chats
                            and chat.get("proof") == before.get("serving"))
            paused = ((name == "sessions" and original.get("id") in paused_chats)
                      or (name == "terminals" and key in paused_terminals))
            if key not in right:
                changes.append({"kind": name, "id": key,
                                "change": "verified paused by update" if paused else "disappeared",
                                "regression": not paused})
                continue
            if (name == "panes" and original.get("session_id") in paused_chats
                    and original.get("content") != right[key].get("content")):
                changes.append({"kind": name, "id": key,
                                "change": "paused chat descriptor changed", "regression": True})
            a, b = project(original, name), project(right[key], name)
            for field in sorted(a.keys() | b.keys()):
                if a.get(field) != b.get(field):
                    # Provider activity may finish while we measure. Preserve
                    # the change in the report without calling idle a loss.
                    healthy_state = field == "state" and str(b.get(field)).lower() in {"running", "idle", "working", "waiting", "succeeded"}
                    resumed_id = name == "sessions" and field == "id"
                    # Titles follow live content: a shell's directory, a
                    # provider-named thread, the focused pane in a window's
                    # title. Only a title that goes blank was lost.
                    retitled = field == "title" and (name == "windows" or bool(b.get(field)))
                    verified_pause = ((paused and field in {"state", "failure", "exit", "attached"})
                                      or (paused_agent and field == "state"))
                    changes.append({"kind": name, "id": key, "change": field,
                                    "before": a.get(field), "after": b.get(field),
                                    "regression": not (healthy_state or resumed_id or retitled or verified_pause),
                                    **({"disposition": "verified paused by update"} if verified_pause else {})})
        for key in sorted(right.keys() - left.keys()):
            changes.append({"kind": name, "id": key, "change": "added", "regression": False})
    current = {(row["pid"], row["start_ticks"]): row for row in after["processes"]}
    # The explicit long-lived workload must survive, rather than restart with
    # the same command. Short-lived provider/CLI processes may legitimately end.
    for row in before["processes"]:
        if (row.get("role") == "long-lived-child" or
                (row.get("terminal_id") and len(row.get("argv", [])) == 2
                 and Path(row["argv"][0]).name == "sleep" and row["argv"][1] == "86400")):
            surviving = current.get((row["pid"], row["start_ticks"]))
            if surviving is None or surviving.get("state") in {"Z", "X"}:
                changes.append({"kind": "terminal-child", "id": row.get("terminal_id") or "child.pid",
                                "change": "long-lived child lost or PID identity changed",
                                "before": row, "regression": True})
    return changes


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    capture = commands.add_parser("record")
    capture.add_argument("root", type=Path)
    capture.add_argument("out", type=Path)
    capture.add_argument("--windows", type=Path)
    recovery = commands.add_parser("prepare-recovery")
    recovery.add_argument("root", type=Path)
    recovery.add_argument("out", type=Path)
    comparison = commands.add_parser("compare")
    comparison.add_argument("before", type=Path)
    comparison.add_argument("after", type=Path)
    comparison.add_argument("--json", action="store_true")
    args = parser.parse_args()
    try:
        if args.command == "prepare-recovery":
            return 0 if prepare_recovery(args.root.resolve(), args.out) else 1
        if args.command == "record":
            snapshot = record(args.root.resolve(), args.out, args.windows)
            print(args.out / "snapshot.json")
            return int(bool(snapshot["errors"]))
        changes = compare(load_snapshot(args.before), load_snapshot(args.after))
        if args.json:
            print(dump(changes), end="")
        else:
            for change in changes:
                detail = ""
                if "before" in change:
                    detail = f" {json.dumps(change['before'], sort_keys=True)} -> {json.dumps(change.get('after'), sort_keys=True)}"
                if "disposition" in change:
                    detail += f" ({change['disposition']})"
                print(f"{'REGRESSION' if change['regression'] else 'CHANGE'} {change['kind']} {change['id']}: {change['change']}{detail}")
            if not changes:
                print("No lost items or state changes.")
        return int(any(change["regression"] for change in changes))
    except (OSError, ValueError, KeyError, TypeError) as error:
        print(f"rehearse-update records: {error}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
