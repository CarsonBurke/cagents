#!/usr/bin/env python3
"""Observe a prepared native transfer in private SQLite stores; never start the app.

Run before the driver's go marker, then after transfer and after relaunch with
the original --before proof. Each database is observed in one read transaction;
the databases do not form a cross-process atomic snapshot. The driver must be
held at the corresponding native evidence boundary.
"""

import argparse
from contextlib import contextmanager
import hashlib
import json
import math
import os
from pathlib import Path
import re
import sqlite3
import stat
import struct
import sys
import time
import uuid


class Refused(ValueError):
    """The supplied fixture or observed state does not establish this proof."""


MAX_BYTES = 4 * 1024 * 1024
MAX_ROWS = 100_000
ROLES = ("active_source", "inactive_source")


def require(condition, message):
    if not condition:
        raise Refused(message)


def unique_object(items):
    result = {}
    for key, value in items:
        require(key not in result, "Duplicate JSON field: " + key)
        result[key] = value
    return result


def decode(value):
    require(isinstance(value, str) and len(value.encode()) <= MAX_BYTES, "Oversized or non-text JSON")
    return json.loads(value, object_pairs_hook=unique_object,
                      parse_constant=lambda value: (_ for _ in ()).throw(Refused("Nonfinite JSON")))


def identifier(value):
    require(isinstance(value, str), "Identity must be a UUID string")
    try:
        parsed = uuid.UUID(value)
    except ValueError as error:
        raise Refused("Invalid UUID identity") from error
    require(str(parsed) == value and parsed.int != 0, "Identity must be a canonical nonnil UUID")
    return value


def unsigned(value):
    require(type(value) is int and 0 <= value < 1 << 64, "Expected an unsigned 64-bit integer")
    return value


def canonical(value, within=None, *, directory=False, missing=False):
    require(isinstance(value, (str, Path)), "Expected a path")
    path = Path(value)
    require(path.is_absolute() and path == path.resolve(strict=not missing), "Path must be canonical and absolute")
    if within is not None:
        require(path.is_relative_to(within) and path != within, "Path escapes its private fixture/task")
        parent = path.parent
        while parent.is_relative_to(within):
            info = parent.stat()
            require(info.st_uid == os.getuid() and not info.st_mode & 0o022, "Unsafe writable fixture directory")
            if parent == within:
                break
            parent = parent.parent
    if not missing:
        info = path.stat()
        require(info.st_uid == os.getuid(), "Path must belong to the current fixture owner")
        require(stat.S_ISDIR(info.st_mode) if directory else stat.S_ISREG(info.st_mode), "Wrong path type")
    return path


def evidence_path(value, task, root, *, missing=False):
    path = canonical(value, task, missing=missing)
    require(not path.is_relative_to(root), "Evidence must be outside the fixture")
    return path


def read_json(path):
    require(path.stat().st_size <= MAX_BYTES, "Evidence file is too large")
    return decode(path.read_text())


def file_identity(path):
    info = path.stat()
    require(stat.S_ISREG(info.st_mode), "Database is not a regular file")
    return {"device": info.st_dev, "inode": info.st_ino}


def rows(db, query, parameters=()):
    result = db.execute(query, parameters).fetchmany(MAX_ROWS + 1)
    require(len(result) <= MAX_ROWS, "Database observation exceeds the row bound")
    return result


@contextmanager
def readonly(entry, root, deadline, schema):
    path = canonical(entry["path"], root)
    identity = {key: unsigned(entry[key]) for key in ("device", "inode")}
    require(file_identity(path) == identity, "Database identity changed")
    db = sqlite3.connect(path.as_uri() + "?mode=ro", uri=True, timeout=0.25)
    db.row_factory = sqlite3.Row
    try:
        db.execute("PRAGMA query_only=ON")
        db.execute("PRAGMA trusted_schema=OFF")
        db.set_progress_handler(lambda: int(time.monotonic() >= deadline), 1000)
        db.execute("BEGIN")
        for table, columns in schema.items():
            kind = rows(db, "SELECT type,sql FROM sqlite_schema WHERE name=?", (table,))
            require(len(kind) == 1 and kind[0]["type"] == "table"
                    and isinstance(kind[0]["sql"], str)
                    and not kind[0]["sql"].upper().startswith("CREATE VIRTUAL"), "Missing or unsafe table: " + table)
            found = {row["name"] for row in rows(db, "PRAGMA table_info(" + table + ")")}
            require(set(columns) <= found, "Missing schema columns: " + table)
        yield db
        require(time.monotonic() < deadline, "Probe deadline expired")
        require(db.total_changes == 0, "Read-only probe changed a database")
    finally:
        db.close()
    require(file_identity(path) == identity, "Database replaced during observation")


REGISTRY_SCHEMA = {
    "metadata": ("key", "identity"),
    "channels": ("id", "repository", "title", "title_source"),
    "view_scopes": ("id", "channel", "repository", "worktree", "revision"),
    "panes": ("id", "scope", "position", "title", "user_title", "width_fraction",
              "pending_terminal_start", "filing", "kind", "resource", "descriptor_format", "descriptor"),
    "retired_panes": ("scope", "pane", "ordinal"),
}
STORE_SCHEMA = {
    "metadata": ("key", "value"),
    "workspaces": ("workspace_id", "existence_json", "focused_card_id"),
    "projects": ("project_id", "repository_json", "existence_json"),
    "worktrees": ("worktree_id", "project_id", "path_json", "existence_json"),
    "cards": ("card_id", "workspace_id", "existence_json", "kind_tag"),
    "browser_restore": ("card_id", "browser_id"),
}


def pane_key(pane):
    require(isinstance(pane, dict) and set(pane) == {
        "id", "title", "user_title", "pending_terminal_start", "width_fraction", "kind", "scope"
    }, "Incomplete pane descriptor")
    identifier(pane["id"])
    require(isinstance(pane["title"], str)
            and (pane["user_title"] is None or isinstance(pane["user_title"], str))
            and type(pane["pending_terminal_start"]) is bool
            and (pane["scope"] is None or isinstance(pane["scope"], str)), "Invalid pane field types")
    width = pane["width_fraction"]
    require(type(width) in (int, float) and math.isfinite(width) and width > 0, "Invalid pane width")
    kind = pane["kind"]
    # Pane's human-readable serde branch and canonical SQLite descriptor both
    # use CardKind's internal tag. The external enum is the binary IPC format;
    # accepting that shape here would conceal a producer/consumer mismatch.
    require(isinstance(kind, dict) and kind.get("kind") in (
        "browser", "terminal", "chat", "pull_requests", "repository"
    ), "Invalid canonical JSON pane kind")
    if kind["kind"] in ("browser", "terminal", "chat"):
        identifier(kind["id"])
    if kind["kind"] == "browser":
        require(set(kind) == {"kind", "id", "current_url", "history", "history_index", "scroll_x", "scroll_y", "fidelity"},
                "Incomplete browser descriptor")
        require(isinstance(kind["current_url"], str) and bool(kind["current_url"])
                and isinstance(kind["history"], list)
                and all(isinstance(url, str) and url for url in kind["history"]), "Invalid browser URL/history types")
        unsigned(kind["history_index"])
        require(kind["fidelity"] in ("url_only", "history_and_scroll"), "Invalid browser fidelity")
        require(all(type(kind[name]) in (int, float) and math.isfinite(kind[name]) for name in ("scroll_x", "scroll_y")),
                "Invalid browser scroll types")
    # Rust stores width as f64 but compares it as f32. Keep every other JSON
    # field and type; no cross-format or omitted/null normalization is needed.
    result = dict(pane)
    result["width_fraction"] = struct.pack("!f", width).hex()
    return json.dumps(result, sort_keys=True, separators=(",", ":"), allow_nan=False)


def observed_pane(row):
    require(row["descriptor_format"] == 1, "Unknown canonical descriptor format")
    descriptor = decode(row["descriptor"])
    require(isinstance(descriptor, dict), "Invalid canonical descriptor")
    tag = descriptor.get("kind")
    require(tag in ("browser", "terminal", "chat", "pull_requests", "repository"), "Unknown canonical pane kind")
    require(tag == row["kind"], "Pane kind contradicts its descriptor")
    resource = identifier(descriptor["id"]) if tag in ("browser", "terminal", "chat") else None
    require(resource == row["resource"], "Canonical resource identity changed")
    require(row["pending_terminal_start"] in (0, 1), "Invalid pending-start flag")
    return dict(id=row["id"], title=row["title"], user_title=row["user_title"],
                pending_terminal_start=bool(row["pending_terminal_start"]),
                width_fraction=row["width_fraction"], scope=row["filing"],
                kind=descriptor)


def registry_observation(marker, phase, root, deadline):
    source, destination = marker["source"], marker["destination"]
    target = destination["scope"]["Worktree"]
    with readonly(marker["registry"], root, deadline, REGISTRY_SCHEMA) as db:
        identity = rows(db, "SELECT identity FROM metadata WHERE key=0")
        require(len(identity) == 1 and identity[0][0] == marker["registry"]["identity"], "Registry identity changed")
        channel = rows(db, "SELECT repository,title,title_source FROM channels WHERE id=?", (source["channel"],))
        require(len(channel) == 1 and tuple(channel[0]) == (source["repository"], source["title"], 2), "Source manual channel identity changed")
        observations = {}
        for name, expected in (("source", source), ("destination", destination)):
            scopes = (rows(db, "SELECT id,revision,channel,repository,worktree FROM view_scopes WHERE channel=?", (source["channel"],))
                      if name == "source" else rows(db, "SELECT id,revision,channel,repository,worktree FROM view_scopes WHERE repository=? AND worktree=?", (target["repository"], target["path"])))
            require(len(scopes) == 1, "Expected one canonical " + name + " scope")
            scope_id, revision, channel_id, repository, worktree = scopes[0]
            shape = (source["channel"], None, None) if name == "source" else (None, target["repository"], target["path"])
            require((channel_id, repository, worktree) == shape, "Canonical scope identity is inconsistent")
            actual = [observed_pane(row) for row in rows(db, "SELECT * FROM panes WHERE scope=? ORDER BY position,id", (scope_id,))]
            retired = [row[0] for row in rows(db, "SELECT pane FROM retired_panes WHERE scope=? ORDER BY ordinal", (scope_id,))]
            wanted = expected["panes"]
            if phase != "before":
                moved = next(pane for pane in source["panes"] if pane["id"] == marker["moved"])
                if name == "source":
                    wanted = [pane for pane in wanted if pane["id"] != marker["moved"]]
                else:
                    placed = next((pane for pane in actual if pane["id"] == marker["moved"]), None)
                    require(placed is not None and placed["scope"] in (None, target["path"]), "Moved canonical filing is wrong")
                    wanted = [*wanted, dict(moved, scope=placed["scope"])]
            require([pane_key(pane) for pane in actual] == [pane_key(pane) for pane in wanted], "Canonical " + name + " descriptors/order changed")
            unsigned(revision)
            require(revision == expected["revision"] if phase == "before" else revision > expected["revision"], "Stale canonical revision")
            expected_retired = expected["retired"]
            if phase != "before" and name == "source":
                expected_retired = [*expected_retired, marker["moved"]]
            require(retired == expected_retired, "Canonical retirement changed")
            observations[name] = dict(scope_id=scope_id, revision=revision, panes=actual, retired=retired)
        owners = rows(db, "SELECT s.channel,s.repository,s.worktree FROM panes p JOIN view_scopes s ON s.id=p.scope WHERE p.id=?", (marker["moved"],))
        expected_owner = (source["channel"], None, None) if phase == "before" else (None, target["repository"], target["path"])
        require(len(owners) == 1 and tuple(owners[0]) == expected_owner, "Moved pane does not have exactly one correct canonical owner")
        return observations


def store_observation(entry, marker, phase, root, deadline):
    source, target = marker["source"], marker["destination"]["scope"]["Worktree"]
    with readonly(entry, root, deadline, STORE_SCHEMA) as db:
        require(db.execute("PRAGMA user_version").fetchone()[0] == 13, "Unsupported window-store schema")
        all_channels = []
        for row in rows(db, "SELECT key,value FROM metadata WHERE key LIKE 'workspace:%:navigation:threads'"):
            match = re.fullmatch(r"workspace:([^:]+):navigation:threads", row["key"])
            require(match is not None, "Malformed workspace navigation key")
            workspace = identifier(match[1])
            channels = decode(row["value"])
            require(isinstance(channels, list), "Channels must be a list")
            all_channels.extend((workspace, channel) for channel in channels)
        sources = [(workspace, channel) for workspace, channel in all_channels if channel.get("id") == source["channel"]]
        require(len(sources) == 1, "Source channel is missing or ambiguous in a window")
        workspace, channel = sources[0]
        project = identifier(channel["project_id"])
        require(channel.get("title") == source["title"] and channel.get("title_source") == "Manual", "Local source channel identity changed")
        projects = rows(db, "SELECT repository_json,existence_json FROM projects WHERE project_id=?", (project,))
        require(len(projects) == 1 and decode(projects[0][0]) == source["repository"] and decode(projects[0][1]) == "open", "Local project identity changed")
        worktrees = [row for row in rows(db, "SELECT worktree_id,project_id,path_json FROM worktrees WHERE existence_json=?", ('"open"',)) if decode(row["path_json"]) == target["path"]]
        require(len(worktrees) == 1 and worktrees[0]["project_id"] == project, "Destination is not one existing same-project worktree")
        worktree = identifier(worktrees[0]["worktree_id"])
        current = rows(db, "SELECT existence_json,focused_card_id FROM workspaces WHERE workspace_id=?", (workspace,))
        require(len(current) == 1 and decode(current[0][0]) == "open", "Workspace is not open")
        def metadata(name):
            found = rows(db, "SELECT value FROM metadata WHERE key=?", (f"workspace:{workspace}:navigation:{name}",))
            require(len(found) == 1, "Missing workspace metadata: " + name)
            return decode(found[0][0])
        require(metadata("workspace_model_identity") == marker["registry"]["identity"], "Window is bound to another registry")
        active, active_worktree, scopes = metadata("active_thread"), metadata("active_worktree"), metadata("card_scopes")
        focus = current[0][1]
        if active is not None:
            identifier(active)
            require(sum(owner == workspace and other["id"] == active for owner, other in all_channels) == 1, "Active channel identity is stale")
        if focus is not None:
            identifier(focus)
            focused_card = rows(db, "SELECT workspace_id,existence_json FROM cards WHERE card_id=?", (focus,))
            require(len(focused_card) == 1 and tuple(focused_card[0]) == (workspace, '"open"'), "Global focus identity is stale")
        wanted_focus = marker["moved"] if phase == "before" else marker["sibling"]
        wanted_cards = [pane["id"] for pane in source["panes"]] if phase == "before" else [marker["sibling"]]
        require(channel["cards"] == wanted_cards and channel["focused"] == wanted_focus, "Source membership/focus is wrong")
        moved_placements = [(owner, other["id"]) for owner, other in all_channels if marker["moved"] in other["cards"]]
        require(moved_placements == ([(workspace, source["channel"])] if phase == "before" else []), "Moved pane has stale/duplicate channel placement")
        sibling_placements = [(owner, other["id"]) for owner, other in all_channels if marker["sibling"] in other["cards"]]
        require(sibling_placements == [(workspace, source["channel"])], "Sibling has duplicate channel placement")
        if entry["role"] == "active_source":
            require(active == source["channel"] and focus == wanted_focus, "Active source global focus is wrong")
        else:
            require(active != source["channel"], "Inactive source became active")
            if phase == "before":
                require(focus != marker["moved"], "Inactive source globally focused the moved pane")
            elif focus == marker["moved"]:
                require(active is None and active_worktree == target["path"], "Moved global focus is outside its destination")
        require(isinstance(scopes, dict), "Card scopes must be a map")
        open_cards = {row[0] for row in rows(db, "SELECT card_id FROM cards WHERE workspace_id=? AND existence_json=?", (workspace, '"open"'))}
        if active is not None:
            selected = next(other for owner, other in all_channels if owner == workspace and other["id"] == active)
            selected_cards = selected["cards"]
            require(isinstance(selected_cards, list) and set(selected_cards) <= open_cards, "Selected channel contains stale identities")
        else:
            canonical(active_worktree, root, directory=True)
            selected_worktrees = [row for row in rows(db, "SELECT path_json FROM worktrees WHERE existence_json=?", ('"open"',))
                                  if decode(row[0]) == active_worktree]
            require(len(selected_worktrees) == 1, "Selected worktree identity is stale")
            selected_cards = [card for card in open_cards if scopes.get(card) == active_worktree]
        require((focus in selected_cards) if focus is not None else not selected_cards,
                "Global focus is not a member of the selected scope")
        require(scopes.get(marker["sibling"]) is None, "Sibling filing changed")
        require(scopes.get(marker["moved"]) == (None if phase == "before" else target["path"]), "Moved local filing is wrong")
        destination_cards = sorted(row[0] for row in rows(db, "SELECT card_id FROM cards WHERE workspace_id=? AND existence_json=?", (workspace, '"open"'))
                                   if scopes.get(row[0]) == target["path"])
        expected_destination = [pane["id"] for pane in marker["destination"]["panes"]]
        if phase != "before":
            expected_destination.append(marker["moved"])
        require(destination_cards == sorted(expected_destination), "Local destination occupants changed")
        for pane in source["panes"]:
            card = rows(db, "SELECT workspace_id,existence_json,kind_tag FROM cards WHERE card_id=?", (pane["id"],))
            require(len(card) == 1 and tuple(card[0]) == (workspace, '"open"', "browser"), "Source browser card identity changed")
            browser = rows(db, "SELECT browser_id FROM browser_restore WHERE card_id=?", (pane["id"],))
            require(len(browser) == 1 and browser[0][0] == pane["kind"]["id"], "Local browser payload identity changed")
        return dict(role=entry["role"], path=entry["path"], device=entry["device"], inode=entry["inode"],
                    workspace_id=workspace, project_id=project, worktree_id=worktree,
                    source_cards=channel["cards"], source_focused=channel["focused"],
                    active_channel=active, active_worktree=active_worktree, global_focused=focus,
                    destination_selected=active is None and active_worktree == target["path"],
                    destination_cards=destination_cards,
                    worktree_focus=metadata("worktree_focus"), moved_scope=scopes.get(marker["moved"]))


def observe(root, prepared, phase, before=None, timeout=5):
    require(phase in ("before", "after", "reopened"), "Unknown probe phase")
    require(0 < timeout <= 30, "Use a bounded timeout of at most 30 seconds")
    deadline = time.monotonic() + timeout
    root = canonical(root, directory=True)
    task = root.parent
    require(root.name == "fixture" and task.stat().st_uid == os.getuid()
            and not (task.stat().st_mode | root.stat().st_mode) & 0o022, "Expected a private owned fixture directory")
    canonical(task / ".native-evidence")
    prepared = evidence_path(prepared, task, root)
    require(prepared.stat().st_size <= MAX_BYTES, "Evidence file is too large")
    prepared_bytes = prepared.read_bytes()
    marker = decode(prepared_bytes.decode())
    require(marker["schema"] == 1 and marker["event"] == "focused_transfer_prepared", "Wrong prepared marker schema/event")
    identifier(marker["nonce"])
    require(marker["fixture_root"] == str(root), "Prepared fixture identity changed")
    namespace = marker["namespace"]
    require(re.fullmatch(r"cagents-development(?:-[A-Za-z0-9][A-Za-z0-9-]{0,23})?", namespace) is not None, "Unsafe fixture namespace")
    data = root / "data" / namespace
    require(marker["registry"]["path"] == str(data / "workspace-model.sqlite3"), "Wrong registry database path")
    identifier(marker["registry"]["identity"])
    require(isinstance(marker["stores"], list) and [entry["role"] for entry in marker["stores"]] == list(ROLES), "Expected two distinct ordered window roles")
    expected_paths = (data / "sessions.sqlite3", data / "windows/window-2/sessions.sqlite3")
    identities = []
    for entry, expected in zip(marker["stores"], expected_paths):
        require(entry["path"] == str(expected), "Wrong window database path")
    for entry in [marker["registry"], *marker["stores"]]:
        path = canonical(entry["path"], root)
        require(file_identity(path) == {key: unsigned(entry[key]) for key in ("device", "inode")}, "Database identity changed")
        identities.append((entry["device"], entry["inode"]))
    require(len(set(identities)) == 3, "Fixture databases must be distinct, not hardlinked")
    source, destination = marker["source"], marker["destination"]
    identifier(source["channel"])
    require(source["title"] == "Focused transfer proof" and source["scope"] == {"Channel": source["channel"]}, "Wrong source fixture scope")
    repository = canonical(source["repository"], root, directory=True)
    require(set(destination["scope"]) == {"Worktree"}, "Wrong destination scope kind")
    target = destination["scope"]["Worktree"]
    require(target["repository"] == str(repository), "Destination belongs to another project")
    canonical(target["path"], root, directory=True)
    require(identifier(marker["moved"]) != identifier(marker["sibling"]), "Moved and sibling identities must differ")
    require([pane["id"] for pane in source["panes"]] == [marker["sibling"], marker["moved"]], "Expected the exact two prepared source panes")
    for scope in (source, destination):
        unsigned(scope["revision"])
        require(isinstance(scope["retired"], list), "Expected retired identities")
        for retired in scope["retired"]:
            identifier(retired)
        for pane in scope["panes"]:
            pane_key(pane)
        pane_ids = [pane["id"] for pane in scope["panes"]]
        require(len(pane_ids) == len(set(pane_ids)) and len(scope["retired"]) == len(set(scope["retired"]))
                and not set(pane_ids).intersection(scope["retired"]), "Duplicate or stale scope identities")
    require(all(pane["kind"]["kind"] == "browser" and pane["scope"] is None for pane in source["panes"]), "Source must contain two unfiled browser panes")
    require(not ({marker["moved"], marker["sibling"]} & {pane["id"] for pane in destination["panes"]}), "Destination already contains a source identity")
    go = prepared.with_suffix(".go")
    expected_go = dict(schema=1, event="focused_transfer_go", nonce=marker["nonce"],
                       registry=marker["registry"]["identity"], source_channel=source["channel"],
                       sibling=marker["sibling"], moved=marker["moved"], destination_scope=destination["scope"])
    baseline = None
    digest = hashlib.sha256(prepared_bytes).hexdigest()
    if phase == "before":
        require(before is None and not go.exists() and not go.is_symlink(), "Before proof must precede the go marker")
    else:
        require(before is not None, "After/reopened requires the original before proof")
        require(read_json(evidence_path(go, task, root)) == expected_go, "Go marker does not match the prepared transfer")
        baseline = read_json(evidence_path(before, task, root))
        require(baseline.get("schema") == 1 and baseline.get("event") == "focused_transfer_probe"
                and baseline.get("phase") == "before" and baseline.get("nonce") == marker["nonce"]
                and baseline.get("prepared_sha256") == digest and baseline.get("prepared_path") == str(prepared)
                and baseline.get("fixture_root") == str(root)
                and baseline.get("registry_identity") == marker["registry"]["identity"], "Stale or unrelated before proof")
    registry = registry_observation(marker, phase, root, deadline)
    stores = [store_observation(entry, marker, phase, root, deadline) for entry in marker["stores"]]
    if baseline is not None:
        for original, current in zip(baseline["stores"], stores, strict=True):
            require(all(current[key] == original[key] for key in ("role", "path", "device", "inode", "workspace_id", "project_id", "worktree_id")), "Window/store/local identity changed since before proof")
    require(hashlib.sha256(prepared.read_bytes()).hexdigest() == digest, "Prepared marker changed during observation")
    return dict(schema=1, event="focused_transfer_probe", phase=phase, nonce=marker["nonce"],
                fixture_root=str(root), prepared_path=str(prepared), prepared_sha256=digest,
                registry_identity=marker["registry"]["identity"], registry=registry, stores=stores, go=expected_go,
                evidence_scope="Read-only durable membership, ownership and focus; native interaction is separate evidence")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ("root", "prepared", "output"):
        parser.add_argument("--" + name, required=True, type=Path)
    parser.add_argument("--phase", choices=("before", "after", "reopened"), required=True)
    parser.add_argument("--before", type=Path)
    parser.add_argument("--timeout", type=float, default=5)
    args = parser.parse_args()
    try:
        result = observe(args.root, args.prepared, args.phase, args.before, args.timeout)
        root = args.root.resolve(strict=True)
        output = evidence_path(args.output, root.parent, root, missing=True)
        with output.open("x") as stream:
            stream.write(json.dumps(result, indent=2, allow_nan=False) + "\n")
    except (Refused, OSError, sqlite3.Error, KeyError, TypeError, ValueError, OverflowError) as error:
        print(json.dumps({"event": "focused_transfer_probe_refused", "error": str(error)}), file=sys.stderr)
        return 2
    print(json.dumps(result, allow_nan=False))
    return 0


if __name__ == "__main__":
    sys.exit(main())
