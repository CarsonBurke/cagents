#!/usr/bin/python3
"""One fixed issue1938 Accounts native Before/After pair. Unbound source, NOT permission.

There is no arbitrary command/input API. The maintained engine owns every child;
the frozen controller owns the fixed gestures; namespace.py owns separate exclusive revision-suffixed state.
The source plan intentionally refuses before effects until its real bindings exist.
"""
import argparse
import datetime
import fcntl
import hashlib
import json
import os
from pathlib import Path
import re
import runpy
import select
import shutil
import signal
import socket
import stat
import subprocess
import sys
import time

PLAN_SHA256 = "a41ff691d544ffc3437a923898c9862691733f4b8cd7e0ac336e794e856438d8"
HERE = Path(__file__).resolve().parent
ROOT = Path("/tmp/coord16-1938-native-ui-log-corrected-1f1e78ab/owned-native")
CHECKOUT = Path("/home/marvin/Documents/repositories/cagents/.worktrees/validation-three-lanes/lane-c-checkout")
COMMON = Path("/home/marvin/Documents/repositories/cagents/.worktrees/fw16-backend-control-ec204421/.git")
TARGET = CHECKOUT.parent / "lane-c-target"
LOCK = CHECKOUT.parent / "lane-c.lock"
OWNER = "validation-lane-c"
ENGINE_SHA = "d9eeec3f2e228e4844695de096f85ecf465381ebd0a2c7a85e964b7103ebe0d9"
HELPER_SHA = "d0c11eb5630112f09b760bb9fd11487e002c665b33e779ba7372007e812bde74"
CONTROLLER_SHA = "6c6f29622c26657f7ca9b355383dc79a2176f92b0444e927b938ebbff17e3714"
NAMESPACE_SHA = "20d2bb1dd58c690c6c480ca20812f342daa5de99c3896e13911341b320474ff2"
SOURCE_SHA = "75843169bba188989984e22d72471100a178a2f5d9c8ac4b7bd164616bdf0e77"
PHASES_SHA = "2a09badabaabd2aa377eaefb0e4b5d029926498cf32d49f723df273e06aca2e1"
COMPILER_BIN = Path("/home/marvin/.rustup/toolchains/1.93.0-x86_64-unknown-linux-gnu/bin")
ZIG = Path("/home/marvin/.local/share/mise/installs/zig/0.15.2/zig")
COMPILER_OVERLAY = {"prepend_path": [str(ZIG.parent), str(COMPILER_BIN)], "CAGENTS_ZIG": str(ZIG)}
GRAPHS = (("development", "cagents"), ("development,host", "cagentsd"),
          ("development,ui", "cagents-ui"))
# Only these window-system routing names are removed from the NEW owned X11
# children. The wrapper, explicit prebuilds and checkout helper retain window routes.
# Values are never logged or used to select authority/account/provider context.
X11_CHILD_REMOVALS = ("WAYLAND_DISPLAY", "WAYLAND_SOCKET", "XDG_ACTIVATION_TOKEN", "DESKTOP_STARTUP_ID")
X11_ROUTING = {"policy": "owned-x11-children-only", "removed_names": list(X11_CHILD_REMOVALS),
               "wrapper_and_explicit_prebuild_window_routes": "unchanged",
               "normal_runner_warm_builds": "inherit-owned-ui-environment",
               "account_authority": "opaque-preserved"}
# All other conflicting authority/application/Git routing names still refuse.
FORBIDDEN = frozenset(("GIT_ATTR_SOURCE", "CAGENTS_DEVELOPMENT_ACCOUNT_FIXTURE",
    "CAGENTS_DEVELOPMENT_AUTOMATION_FIXTURE", "CAGENTS_DEVELOPMENT_NATIVE_IDENTITY_FIXTURE",
    "CAGENTS_CHAT_STAGE_OUTPUT", "CAGENTS_UI_LAUNCH_HANDSHAKE", "CAGENTS_UI_LAUNCH_RECOVERY_PROGRESS",
    "CAGENTS_UI_UPDATE_GUARD_FD", "CAGENTS_DAEMON_LAUNCH_FENCE_FD",
    "CAGENTS_DEVELOPMENT_DAEMON_RECOVERY_FIXTURE", "CAGENTS_LATENCY_OUTPUT",
    "CAGENTS_INSTALL_RESULT_FD", "CAGENTS_INSTALL_OWNER_CLI", "CAGENTS_ARCHIVE",
    "CAGENTS_CHECKSUMS", "CAGENTS_REPOSITORY",
    "GIT_DIR", "GIT_WORK_TREE", "GIT_COMMON_DIR", "GIT_INDEX_FILE", "GIT_CONFIG",
    "GIT_CONFIG_COUNT", "GIT_CONFIG_PARAMETERS", "GIT_CONFIG_SYSTEM", "GIT_CONFIG_GLOBAL",
    "GIT_CONFIG_NOSYSTEM", "GIT_OBJECT_DIRECTORY", "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_CEILING_DIRECTORIES", "GIT_DISCOVERY_ACROSS_FILESYSTEM", "RUSTC_WRAPPER",
    "RUSTC_WORKSPACE_WRAPPER", "RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS"))
CANCELLED = False
C_LOCK_FD = None


def require(value, message):
    if not value:
        raise RuntimeError(message)


def pairs(items):
    result = {}
    for key, value in items:
        require(key not in result, "Duplicate JSON member")
        result[key] = value
    return result


def identity(path):
    info = Path(path).lstat()
    require(stat.S_ISDIR(info.st_mode) and info.st_uid == os.getuid()
            and not info.st_mode & 0o022 and Path(path).resolve(strict=True) == Path(path),
            "Unsafe or replaced directory")
    return {"device": info.st_dev, "inode": info.st_ino}


def signature(info):
    return (info.st_dev, info.st_ino, info.st_mode, info.st_uid, info.st_nlink,
            info.st_size, info.st_mtime_ns, info.st_ctime_ns)


def read_bytes(path, limit=16777216, private=False, owned_input=False):
    path = Path(path)
    require(path.is_absolute() and path.resolve(strict=True) == path, "Symlink/relative file refused")
    fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_CLOEXEC)
    try:
        before = os.fstat(fd)
        require(stat.S_ISREG(before.st_mode) and before.st_size <= limit, "Not a bounded regular file")
        if private:
            require(before.st_uid == os.getuid() and stat.S_IMODE(before.st_mode) == 0o600
                    and before.st_nlink == 1, "Private original receipt required")
        if owned_input:
            require(before.st_uid == os.getuid() and before.st_nlink == 1
                    and not before.st_mode & 0o022,
                    "Immutable input must be owned, single-link and not writable by other users")
        chunks, remaining = [], limit + 1
        while remaining:
            part = os.read(fd, min(remaining, 1048576))
            if not part:
                break
            chunks.append(part)
            remaining -= len(part)
        data = b"".join(chunks)
        after = os.fstat(fd)
        require(len(data) <= limit and signature(before) == signature(after)
                == signature(path.lstat()), "File changed while read")
        return data
    finally:
        os.close(fd)


def fixture_log_identity(fd, path):
    """The original UI redirection inode, never the runner's stdout."""
    info, named = os.fstat(fd), Path(path).lstat()
    require(stat.S_ISREG(info.st_mode) and stat.S_ISREG(named.st_mode)
            and info.st_uid == os.getuid() and stat.S_IMODE(info.st_mode) == 0o600
            and info.st_nlink == 1, "Original UI log must be private, regular and single-link")
    key = lambda item: (item.st_dev, item.st_ino, item.st_uid, item.st_mode, item.st_nlink)
    require(key(info) == key(named), "Original UI log path/incarnation changed")
    return {"path": str(path), "device": info.st_dev, "inode": info.st_ino,
            "uid": info.st_uid, "mode": stat.S_IMODE(info.st_mode), "links": info.st_nlink}


def fixture_log_bytes(fd, reference):
    require(type(reference) is dict and set(reference) == {"path", "device", "inode", "uid", "mode", "links"}
            and type(reference["path"]) is str and Path(reference["path"]).is_absolute()
            and all(type(reference[name]) is int for name in ("device", "inode", "uid", "mode", "links")),
            "Invalid original UI log reference")
    path = Path(reference["path"])
    before = os.fstat(fd)
    require(fixture_log_identity(fd, path) == reference and before.st_size <= 4194304,
            "Original UI log identity/size changed")
    chunks, offset = [], 0
    while offset <= 4194304:
        part = os.pread(fd, min(1048576, 4194305 - offset), offset)
        if not part:
            break
        chunks.append(part)
        offset += len(part)
    raw = b"".join(chunks)
    stamp = lambda item: (item.st_dev, item.st_ino, item.st_mode, item.st_uid, item.st_nlink,
                          item.st_size, item.st_mtime_ns, item.st_ctime_ns)
    require(len(raw) == before.st_size and len(raw) <= 4194304
            and stamp(before) == stamp(os.fstat(fd)) == stamp(path.lstat())
            and fixture_log_identity(fd, path) == reference, "Original UI log changed while read")
    return raw


def fixture_log_events(raw, scene):
    require(type(raw) is bytes and len(raw) <= 4194304
            and (not raw or raw.endswith(b"\n")), "Partial/oversize original UI log; no input/retry")
    events = []
    for line in raw.splitlines():
        if line.startswith(b"ACCOUNT_NATIVE "):
            event = json.loads(line[len(b"ACCOUNT_NATIVE "):], object_pairs_hook=pairs)
            require(type(event) is dict and type(event.get("event")) is str, "Invalid original fixture event")
            require(event.get("scene", scene) == scene, "Original scene changed")
            require(event["event"] not in ("fixture_error", "step_refused"), "Original fixture refused/stopped")
            events.append(event)
            require(len(events) <= 4096, "Original fixture event bound exceeded")
    require(events, "No original fixture evidence")
    return events


def digest(path, limit=8589934592):
    # Stream artifacts; do not hold multi-GiB ELFs in the adapter's memory.
    path = Path(path)
    require(path.is_absolute() and path.resolve(strict=True) == path, "Artifact path must be physical")
    fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_CLOEXEC)
    try:
        before = os.fstat(fd)
        require(stat.S_ISREG(before.st_mode) and before.st_size <= limit, "Artifact size/type refused")
        h = hashlib.sha256()
        while True:
            data = os.read(fd, 1048576)
            if not data:
                break
            h.update(data)
        after = os.fstat(fd)
        require(signature(before) == signature(after) == signature(path.lstat()), "Artifact changed while hashing")
        return h.hexdigest()
    finally:
        os.close(fd)


def pinned(reference, private=False, owned_input=False):
    require(type(reference) is dict and set(reference) == {"path", "sha256"}
            and re.fullmatch(r"[0-9a-f]{64}", reference["sha256"]), "Unbound immutable input")
    data = read_bytes(reference["path"], private=private, owned_input=owned_input)
    require(hashlib.sha256(data).hexdigest() == reference["sha256"], "Immutable input hash changed")
    return json.loads(data, object_pairs_hook=pairs)


def save(path, value):
    path = Path(path)
    require(path.parent.resolve(strict=True) == path.parent and ROOT in path.parents,
            "Output must remain beneath the original capture root")
    fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW | os.O_CLOEXEC, 0o600)
    try:
        with os.fdopen(fd, "w") as output:
            json.dump(value, output, sort_keys=True, indent=2)
            output.write("\n")
            output.flush()
            os.fsync(output.fileno())
    except BaseException:
        # Original partial output is retained. No unlink/replace/retry.
        raise
    return {"path": str(path), "sha256": digest(path)}


def field(document, path):
    require(type(path) is list and path and all(type(p) is str for p in path),
            "Actual allocation schema mapping has not been supplied")
    for part in path:
        require(type(document) is dict and part in document, "Actual allocation field absent")
        document = document[part]
    return document


def build_path(plan):
    require(plan["compiler_overlay"] == COMPILER_OVERLAY, "Reviewed compiler overlay changed")
    # Deliberate compiler selection, declared before any original allocation.
    # No account/HOME values are inspected or changed; the parent is untouched.
    return os.pathsep.join(COMPILER_OVERLAY["prepend_path"] + [os.environ.get("PATH", os.defpath)])


def admission(args):
    require(sys.flags.isolated == 1 and sys.dont_write_bytecode and sys.flags.optimize == 0,
            "Invoke the original Python with -I -B and without optimization")
    plan_path = HERE / "adapter-plan.json"
    raw = read_bytes(plan_path)
    require(hashlib.sha256(raw).hexdigest() == PLAN_SHA256, "Adapter plan changed")
    plan = json.loads(raw, object_pairs_hook=pairs)
    # This is deliberately before component imports, output creation, C or children.
    require(plan["binding_status"] == "BOUND_FOR_INDEPENDENT_REVIEW" and not plan["holds"],
            "INVALID_SOURCE_PLAN_HOLD: " + "; ".join(plan["holds"]))
    require(plan["scene"] == "claude-malformed" and plan["viewport"] == "narrow",
            "This source packet is fixed to claude-malformed-narrow; no workload selector")
    require(plan["source_objects_available_in_execution_common"] is True,
            "Matched source import into the actual execution common remains unbound")
    require(all(name not in FORBIDDEN and not name.startswith(("CARGO_PROFILE_", "GIT_CONFIG_KEY_", "GIT_CONFIG_VALUE_"))
                for name in os.environ), "Conflicting inherited route name; no environment repair")
    require(plan["x11_routing"] == X11_ROUTING, "Owned X11 child routing policy is not bound")
    require(plan["compiler_overlay"] == COMPILER_OVERLAY, "Compiler tool selection is not bound")
    require(plan["compiler_environment_absent"] == ["CARGO_HOME", "RUSTC", "RUSTDOC"]
            and all(name not in os.environ for name in plan["compiler_environment_absent"]),
            "Natural compiler route changed; no environment repair")
    require(plan["cargo_home_route"] == {
                "policy": "opaque-HOME-equality-only", "expected_home": "/home/marvin",
                "expected_cargo_home": "/home/marvin/.cargo"}
            and os.environ.get("HOME") == plan["cargo_home_route"]["expected_home"],
            "Natural HOME no longer selects the reviewed Cargo configuration route")
    require(plan["checkout"] == str(CHECKOUT) and plan["common_git_dir"] == str(COMMON)
            and plan["target"] == str(TARGET) and plan["lock"] == str(LOCK) and plan["owner"] == OWNER,
            "Only the existing canonical C bindings are accepted")
    for name, sha in (("controller.py", CONTROLLER_SHA), ("namespace.py", NAMESPACE_SHA),
                      ("source-binding.json", SOURCE_SHA), ("phases.json", PHASES_SHA)):
        require(digest(HERE / name) == sha, "Frozen component changed: " + name)
    require(type(plan["context_files"]) is list and plan["context_files"], "Build/config context scope unbound")
    require(type(plan["tools"]) is dict and set(plan["tools"]) == {
        "cargo", "rustc", "zig", "python3", "git", "sh", "sed", "dirname", "readlink",
        "xauth", "dbus-daemon", "Xvfb", "libX11", "libXtst", "pillow"}, "Exact tool/library scope unbound")
    require(type(plan["initial_checkout"]) is dict and type(plan["control"]) is dict,
            "Original slot/control binding absent")
    for reference in plan["tools"].values():
        require(type(reference) is dict and set(reference) == {"path", "sha256"}
                and type(reference["path"]) is str and Path(reference["path"]).is_absolute()
                and type(reference["sha256"]) is str and re.fullmatch(r"[0-9a-f]{64}", reference["sha256"]),
                "Tool/library binding incomplete")
    require(plan["tools"]["cargo"]["path"] == str(COMPILER_BIN / "cargo")
            and plan["tools"]["rustc"]["path"] == str(COMPILER_BIN / "rustc")
            and plan["tools"]["zig"]["path"] == str(ZIG),
            "Use the reviewed actual compiler executables, not dispatcher aliases")
    require(type(plan["display"]) is str and re.fullmatch(r":[1-9][0-9]{0,4}", plan["display"])
            and int(plan["display"][1:]) <= 65535 and 0 < plan["case_seconds"] <= 7200
            and 0 < plan["pair_seconds"] <= 14400, "Display/deadline binding incomplete")
    require(set(plan["expected_cargo_features"]) == {b for _, b in GRAPHS}
            and all(type(v) is list and v and all(type(s) is str for s in v)
                    for v in plan["expected_cargo_features"].values()), "Exact source-derived Cargo features unbound")
    source_pair = json.loads(read_bytes(HERE / "source-binding.json"), object_pairs_hook=pairs)["sides"]
    require(plan["pair"] == {side: {"head": source_pair[side]["head"], "tree": source_pair[side]["tree"]}
                             for side in ("before", "after")}, "Exact matched source pair required")
    initial = plan["initial_checkout"]
    require(set(initial) == {"head", "tree", "registry", "checkout_identity", "accepted_prior_return"}
            and all(type(initial[k]) is str and re.fullmatch(r"[0-9a-f]{40}", initial[k]) for k in ("head", "tree")),
            "Actual initial owned-slot profile incomplete; no absent fallback")
    require(plan["control"]["metadata"] and all(k in plan["control"] for k in ("repository", "head", "tree", "identity", "common_identity")),
            "Actual physical published control metadata incomplete")
    require(all(plan["control"][key] == plan["frozen_control_reference"][key]
                for key in ("repository", "head", "tree")),
            "Only the frozen physical control359 source is accepted by this packet")
    require(type(plan["lock_identity"]) is dict and type(plan["target_identity"]) is dict,
            "Actual existing C lock/target identity incomplete")
    for ref in plan["reviewed_inputs"]:
        pinned(ref, owned_input=True)
    require(plan["reviewed_inputs"], "Independent component/source evidence absent")
    grant_ref = {"path": args.allocation, "sha256": args.allocation_sha256}
    grant = pinned(grant_ref, owned_input=True)
    # Mapping is supplied from the ORIGINAL scheduler record, not a fabricated
    # adapter UUID or a new permission schema. Unmapped real records refuse.
    mapping = plan["allocation_fields"]
    expected = {"adapter_sha256": digest(Path(__file__).resolve()), "plan_sha256": PLAN_SHA256,
                "checkout": str(CHECKOUT), "target": str(TARGET), "lock": str(LOCK),
                "owner": OWNER, "scene": plan["scene"], "viewport": plan["viewport"],
                "before": plan["pair"]["before"], "after": plan["pair"]["after"]}
    require(type(mapping) is dict and set(mapping) == set(expected) | {"expires_utc", "allocation_id"},
            "Actual grant schema has not been bound")
    for name, value in expected.items():
        require(field(grant, mapping[name]) == value, "Allocation mismatch: " + name)
    expires = datetime.datetime.fromisoformat(field(grant, mapping["expires_utc"]).replace("Z", "+00:00"))
    require(expires.tzinfo is not None and type(field(grant, mapping["allocation_id"])) is str,
            "Actual original allocation identity/deadline missing")
    remaining = (expires - datetime.datetime.now(datetime.timezone.utc)).total_seconds()
    require(remaining > 0, "Original allocation expired; no retry")
    plan["_grant"] = grant_ref
    plan["_end"] = time.monotonic() + min(remaining, plan["pair_seconds"])
    return plan


def guard(plan):
    require(digest(HERE / "adapter-plan.json") == PLAN_SHA256
            and digest(Path(__file__).resolve()) == plan["adapter_sha256"], "Adapter source changed")
    for name, sha in (("controller.py", CONTROLLER_SHA), ("namespace.py", NAMESPACE_SHA),
                      ("source-binding.json", SOURCE_SHA), ("phases.json", PHASES_SHA)):
        require(digest(HERE / name) == sha, "Frozen component changed")
    for ref in [plan["_grant"], *plan["reviewed_inputs"]]:
        pinned(ref, owned_input=True)
    control = Path(plan["control"]["repository"])
    require(identity(control) == plan["control"]["identity"] and identity(COMMON) == plan["control"]["common_identity"],
            "Original physical control/common changed")
    for ref in plan["control"]["metadata"]:
        require(digest(Path(ref["path"])) == ref["sha256"], "Original control metadata changed")
    for relative, sha in (("scripts/prepare-validation-checkout", HELPER_SHA),
                          ("scripts/qualification/process_custody.py", ENGINE_SHA)):
        require(digest(control / relative) == sha, "Maintained helper/engine changed")
    for name, ref in plan["tools"].items():
        require(digest(Path(ref["path"])) == ref["sha256"], "Admitted tool/library changed: " + name)
        if name in ("cargo", "rustc", "zig", "git", "sed", "dirname", "readlink"):
            chosen = shutil.which(name, path=build_path(plan))
            require(chosen is not None and Path(chosen).resolve() == Path(ref["path"]),
                    "Reviewed build PATH no longer selects admitted tool: " + name)
    require(identity(ROOT) == plan["_root_identity"], "Original capture root changed")


def build_context(plan):
    # Only the reviewed compiler/build-config path scope. No authority/home
    # values are inspected or exported. Missing files are an explicit baseline.
    result = []
    for entry in plan["context_files"]:
        path = Path(entry["path"])
        if entry["sha256"] is None:
            require(not path.exists() and not path.is_symlink(), "Previously absent build config appeared")
        else:
            require(digest(path) == entry["sha256"], "Reviewed build/config context changed")
        result.append(entry)
    return result


class Case:
    def __init__(self, plan, side, engine, *, helper_only=False):
        self.plan, self.side = plan, side
        self.pin = plan["pair"][side]
        self.owner = engine["Custody"](lambda: CANCELLED)
        self.work = ROOT / (side + "-" + plan["scene"] + "-" + plan["viewport"]
                            + ("-final-clean" if helper_only else ""))
        self.work.mkdir(mode=0o700)  # Never reuse a failed or completed case.
        self.work_identity = identity(self.work)
        self.number, self.commands, self.services = 0, [], []
        self.source = None
        self.scene_started, self.helper_only = False, helper_only
        self.settled = False
        self.fixture_log_fd, self.fixture_log_ref = None, None
        self.fixture_log_raw, self.fixture_log_capture, self.fixture_log_error = None, None, None
        self.fixture_log_closed, self.fixture_log_close_attempted = False, False
        self.fixture_log_close_error = None
        self.end = min(plan["_end"], time.monotonic() + plan["case_seconds"])
        # Copy opaque inherited values; do not inspect/clear HOME/account/auth.
        self.base = os.environ.copy()
        self.base.update(CARGO_TARGET_DIR=str(TARGET), CARGO_BUILD_JOBS="2",
                         CAGENTS_ZIG=plan["tools"]["zig"]["path"], PATH=build_path(plan))
        self.records = []

    def event(self, kind, **data):
        self.number += 1
        ref = save(self.work / (f"{self.number:04d}-" + kind + ".json"), data)
        self.records.append(ref)
        return ref

    def check(self):
        require(not CANCELLED and time.monotonic() < self.end, "Original case cancelled/deadline expired")
        guard(self.plan)
        require(identity(self.work) == self.work_identity, "Original case output directory changed")
        if "_context" in self.plan:
            require(build_context(self.plan) == self.plan["_context"], "Paired build context changed")
        if self.source is not None:
            require(identity(CHECKOUT) == self.source["checkout_identity"]
                    and identity(COMMON) == self.source["common_identity"]
                    and identity(self.source["admin_git_dir"]) == self.source["admin_identity"],
                    "Original admitted checkout/common/admin directory changed")
            for path, sha in self.source["git_metadata_sha256"].items():
                require(digest(Path(path)) == sha, "Original source metadata changed")
            for name, sha in self.source["source_files"].items():
                require(digest(CHECKOUT / name) == sha, "Admitted source changed: " + name)

    def command(self, argv, role, seconds, *, env=None):
        self.check()
        before = set(self.owner.nodes)
        self.number += 1
        prefix = self.work / f"{self.number:04d}-{role}"
        out, err = Path(str(prefix) + ".stdout"), Path(str(prefix) + ".stderr")
        record = {"argv": argv, "cwd": str(CHECKOUT), "role": role,
                  "stdout": str(out), "stderr": str(err), "state": "LAUNCHING",
                  "original_exit": None, "cleanup_exit": None, "started_monotonic": time.monotonic()}
        self.commands.append(record)  # Custody of partial launches is retained.
        deadline = min(self.end, time.monotonic() + seconds)
        with out.open("xb", buffering=0) as stdout, err.open("xb", buffering=0) as stderr:
            os.chmod(out, 0o600); os.chmod(err, 0o600)
            child = self.owner.launch(argv, str(CHECKOUT), role, env=self.base if env is None else env,
                                      stdout=stdout, stderr=stderr)
            record["pid"] = child.pid
            while True:
                self.owner.scan()
                if child.poll() is not None:
                    code = child.wait()
                    self.owner.scan()
                    # Include every new observed node conservatively; an
                    # unclassified concurrent descendant cannot be ignored.
                    fresh = set(self.owner.nodes) - before
                    if all(self.owner.nodes[pid].get("settled") for pid in fresh):
                        record.update(state="FINISHED", original_exit=code, finished_monotonic=time.monotonic(),
                                      family=[{k: v for k, v in self.owner.nodes[pid].items() if k != "pidfd"}
                                              for pid in sorted(fresh)])
                        break
                if CANCELLED or time.monotonic() >= deadline:
                    record["state"] = "CANCELLED" if CANCELLED else "TIMEOUT"
                    self.event("original-incomplete", **record)
                    raise RuntimeError("Original " + role + " cancelled/timed out; cleanup is not success")
                time.sleep(0.025)
        self.check()
        record.update(stdout_sha256=digest(out), stderr_sha256=digest(err))
        reference = self.event("command", **record)
        require(record["original_exit"] == 0, "Original " + role + " failed; no retry")
        return record, reference

    def admit(self, expected):
        control = Path(self.plan["control"]["repository"])
        argv = [self.plan["tools"]["python3"]["path"], "-I", "-B",
                str(control / "scripts/prepare-validation-checkout"), "--repository", str(control),
                "--checkout", str(CHECKOUT), "--owner", OWNER, "--head", self.pin["head"],
                "--tree", self.pin["tree"], "--expect-current", expected]
        record, ref = self.command(argv, "checkout-helper", 60)
        receipt = json.loads(read_bytes(record["stdout"], private=True), object_pairs_hook=pairs)
        require(receipt["version"] == 1 and receipt["status"] == "prepared"
                and receipt["operation"] == ("reused" if expected == self.pin["head"] else "updated")
                and receipt["previous_head"] == expected and receipt["head"] == self.pin["head"]
                and receipt["tree"] == self.pin["tree"] and receipt["checkout"] == str(CHECKOUT)
                and receipt["repository"] == str(control) and receipt["common_git_dir"] == str(COMMON)
                and receipt["owner"] == OWNER and receipt["detached"] is True and receipt["clean"] is True
                and receipt["source_only"] is True, "Actual maintained helper receipt mismatch")
        marker = COMMON / "validation-checkouts-v1" / (hashlib.sha256(str(CHECKOUT).encode()).hexdigest() + ".json")
        require(receipt["registry"] == str(marker), "Wrong original registry")
        registry = json.loads(read_bytes(marker, 16384, private=True), object_pairs_hook=pairs)
        require(registry["head"] == self.pin["head"] and registry["tree"] == self.pin["tree"]
                and registry["owner"] == OWNER and registry["uid"] == os.getuid()
                and registry["checkout"] == str(CHECKOUT) and registry["common_git_dir"] == str(COMMON)
                and registry["common_identity"] == [COMMON.stat().st_dev, COMMON.stat().st_ino],
                "Maintained registry did not acknowledge original admission")
        sources = json.loads(read_bytes(HERE / "source-binding.json"), object_pairs_hook=pairs)["sides"][self.side]
        require(sources["head"] == self.pin["head"] and sources["tree"] == self.pin["tree"], "Pair/source mismatch")
        admin = Path(registry["checkout_git_dir"])
        require(admin.parent == COMMON / "worktrees"
                and registry["checkout_git_dir_identity"] == [admin.stat().st_dev, admin.stat().st_ino],
                "Original admin identity mismatch")
        metadata = (CHECKOUT / ".git", admin / "HEAD", admin / "gitdir", admin / "commondir", admin / "index")
        require(read_bytes(CHECKOUT / ".git") == ("gitdir: " + str(admin) + "\n").encode()
                and read_bytes(admin / "HEAD") == (self.pin["head"] + "\n").encode(), "Detached source metadata mismatch")
        self.source = {"checkout_identity": identity(CHECKOUT), "common_identity": identity(COMMON),
                       "admin_git_dir": str(admin), "admin_identity": identity(admin),
                       "git_metadata_sha256": {str(p): digest(p) for p in metadata}, "source_files": sources["files"]}
        self.check()
        self.event("admitted-source", receipt=receipt, original_command=ref, registry=registry,
                   source=self.source, initial_git_and_cargo_were_unrun_before_helper=True)

    def builds(self):
        results, artifacts = [], {}
        for features, binary in GRAPHS:
            argv = ["cargo", "build", "--locked", "--message-format=json", "--profile", "dev-runtime",
                    "--no-default-features", "--features", features, "--bin", binary]
            record, reference = self.command(argv, "build-" + binary, 1200)
            found, finished = [], []
            with open(record["stdout"], "rb") as source:
                for line in source:
                    require(len(line) <= 16777216, "Cargo JSON line exceeds source plan")
                    item = json.loads(line, object_pairs_hook=pairs)
                    if item.get("reason") == "build-finished":
                        finished.append(item)
                    if (item.get("reason") == "compiler-artifact"
                            and item.get("target", {}).get("name") == binary
                            and item.get("target", {}).get("kind") == ["bin"]):
                        found.append(item)
            require(len(found) == 1 and len(finished) == 1 and finished[0].get("success") is True,
                    "Missing/ambiguous actual Cargo artifact/completion")
            artifact = found[0]
            expected_features = self.plan["expected_cargo_features"][binary]
            require(type(expected_features) is list and expected_features
                    and sorted(artifact["features"]) == sorted(expected_features)
                    and artifact["target"]["kind"] == ["bin"] and not artifact["profile"]["test"],
                    "Actual feature/bin graph differs from reviewed isolated graph")
            require(not ("host" in artifact["features"] and "ui" in artifact["features"])
                    and ("host" in artifact["features"]) == (binary == "cagentsd")
                    and ("ui" in artifact["features"]) == (binary == "cagents-ui"),
                    "Isolated CLI/HOST/UI feature boundary violated")
            executable = Path(artifact["executable"])
            require(executable == TARGET / "dev-runtime" / binary
                    and read_bytes_prefix(executable, 4) == b"\x7fELF", "Wrong original ELF artifact")
            artifacts[binary] = {"path": str(executable), "sha256": digest(executable), "cargo": artifact}
            results.append({"argv": argv, "original_exit": record["original_exit"], "receipt": reference})
        require(all(n.get("settled") for n in self.owner.nodes.values()), "Build family remains outstanding")
        self.owner.close_pidfds()  # Owner remains open; no live native family yet.
        self.event("matched-builds", artifacts=artifacts, commands=results,
                   original_build_family_settled=True, warm_runner_builds_still_required=True)
        return results, artifacts

    def service(self, argv, role, env):
        self.check()
        stdout = self.work / (role + ".stdout")
        stderr = self.work / (role + ".stderr")
        with stdout.open("xb", buffering=0) as out, stderr.open("xb", buffering=0) as err:
            os.chmod(stdout, 0o600); os.chmod(stderr, 0o600)
            child = self.owner.launch(argv, str(CHECKOUT), role, env=env, stdout=out, stderr=err)
        record = {"role": role, "pid": child.pid, "argv": argv, "stdout": str(stdout), "stderr": str(stderr)}
        self.services.append(record)
        self.event("service-start", **record)
        return child, record

    def cookie(self, node, expected):
        require(not node.get("settled") and not self.owner.exited(node), "Original scene process exited")
        current = self.owner.current(node, executable=True)
        require(current["executable"] == str(expected)
                and current["executable_identity"] == [expected.stat().st_dev, expected.stat().st_ino],
                "Original scene executable changed")
        node.update(pinned_executable=str(expected), pinned_executable_identity=current["executable_identity"])
        return {"pid": node["pid"], "start_ticks": node["start_ticks"], "uid": node["uid"], "executable": str(expected)}

    def start_native(self, namespace, builds, artifacts):
        require(namespace.purpose == "native" and namespace.scene == self.plan["scene"]
                and namespace.viewport == self.plan["viewport"] and namespace.variant == self.side,
                "Private contract fixtures are never native authority")
        display = self.plan["display"]
        require(type(display) is str and re.fullmatch(r":[1-9][0-9]{0,4}", display)
                and int(display[1:]) <= 65535, "Actual explicit private display unbound")
        socket_path = Path("/tmp/.X11-unix/X" + display[1:])
        require(not socket_path.exists() and not socket_path.is_symlink()
                and not Path("/tmp/.X" + display[1:] + "-lock").exists(), "Occupied display; no adoption/retry")
        auth = self.work / "Xauthority"
        # xauth receives the secret through an original private input file, not
        # argv, shell substitution, environment, receipt or the terminal.
        auth.touch(mode=0o600, exist_ok=False)
        secret = os.urandom(16).hex()
        data_path = self.work / "xauth-private-input"
        fd = os.open(data_path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
        with os.fdopen(fd, "w") as stream:
            stream.write("add " + display + " . " + secret + "\n")
        del secret
        # No shell/input API is added to command(); this one fixed bootstrap
        # operation uses xauth's documented source-file command.
        self.command([self.plan["tools"]["xauth"]["path"], "-q", "-f", str(auth),
                      "source", str(data_path)], "xauth", 10)
        xvfb = ROOT / "Xvfb"
        if self.side == "before":
            with xvfb.open("xb") as out:
                os.chmod(xvfb, 0o700)
                with open(self.plan["tools"]["Xvfb"]["path"], "rb") as source:
                    shutil.copyfileobj(source, out, 1048576)
            require(digest(xvfb) == self.plan["tools"]["Xvfb"]["sha256"], "Owned Xvfb copy differs")
            self.plan["_xvfb_identity"] = [xvfb.stat().st_dev, xvfb.stat().st_ino]
        require([xvfb.stat().st_dev, xvfb.stat().st_ino] == self.plan["_xvfb_identity"], "Owned Xvfb replaced")
        require(digest(xvfb) == self.plan["tools"]["Xvfb"]["sha256"], "Owned Xvfb bytes changed")
        env = {**self.base, "DISPLAY": display, "XAUTHORITY": str(auth)}
        removed = [name for name in X11_CHILD_REMOVALS if name in env]
        for name in X11_CHILD_REMOVALS:
            env.pop(name, None)
        self.event("owned-display-routing", policy=self.plan["x11_routing"],
                   removed_names=removed, natural_wrapper_and_build_context_unchanged=True)
        server, _ = self.service([str(xvfb), display, "-screen", "0", "1920x1200x24",
                                  "-nolisten", "tcp", "-noreset", "-auth", str(auth)], "owned-Xvfb", env)
        deadline = min(self.end, time.monotonic() + 10)
        while not socket_path.exists():
            self.owner.scan()
            require(server.poll() is None and not CANCELLED and time.monotonic() < deadline,
                    "Owned Xvfb failed readiness; no new display/retry")
            time.sleep(0.025)
        server_cookie = self.cookie(self.owner.nodes[server.pid], xvfb)
        info = socket_path.lstat()
        require(stat.S_ISSOCK(info.st_mode) and info.st_uid == os.getuid(), "X server socket is not owned")
        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as connection:
            connection.settimeout(max(0.001, deadline - time.monotonic()))
            connection.connect(str(socket_path))
            import struct
            pid, uid, _ = struct.unpack("3i", connection.getsockopt(socket.SOL_SOCKET, socket.SO_PEERCRED, 12))
            require(pid == server.pid and uid == os.getuid(), "Socket peer is not original Xvfb")
        server_cookie.update(boot_id=self.owner.boot_id,
                             socket={"path": str(socket_path), "device": info.st_dev, "inode": info.st_ino})
        bus_path = self.work / "bus.sock"
        bus, bus_record = self.service([self.plan["tools"]["dbus-daemon"]["path"], "--session", "--nofork",
            "--print-address=1", "--print-pid=1", "--address=unix:path=" + str(bus_path)], "owned-private-bus", env)
        deadline = min(self.end, time.monotonic() + 10)
        while True:
            self.owner.scan()
            require(bus.poll() is None and not CANCELLED and time.monotonic() < deadline, "Private bus failed readiness")
            lines = live_output(bus_record["stdout"], 16384).decode().splitlines()
            if len(lines) == 2 and bus_path.exists():
                break
            time.sleep(0.025)
        require(lines[0].startswith("unix:path=" + str(bus_path)) and lines[1] == str(bus.pid),
                "Bus did not identify its original foreground process/address")
        ui_env = {**env, **namespace.environment_paths(), "DBUS_SESSION_BUS_ADDRESS": lines[0],
                  "CAGENTS_DEVELOPMENT_ACCOUNT_FIXTURE": self.plan["scene"]}
        log = self.work / "ui.log"
        require(stat.S_IMODE(self.work.lstat().st_mode) == 0o700
                and identity(self.work) == self.work_identity, "Original UI log directory changed")
        self.fixture_log_fd = os.open(log, os.O_RDONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW | os.O_CLOEXEC, 0o600)
        self.fixture_log_ref = fixture_log_identity(self.fixture_log_fd, log)
        runner, runner_record = self.service([str(CHECKOUT / "scripts/run-development"), "--browserless", "--debug",
                                  "--log-file", str(log)], "owned-development-runner", ui_env)
        self.runner_pid = runner.pid
        ui_path = Path(artifacts["cagents-ui"]["path"])
        deadline = min(self.end, time.monotonic() + 1200)
        while True:
            self.owner.scan()
            require(not self.owner.exact_matches(artifacts["cagentsd"]["path"]),
                    "Inert fixture unexpectedly launched a daemon; native case refused")
            matches = self.owner.exact_matches(str(ui_path))
            require(len(matches) <= 1, "Ambiguous owned development UI")
            if matches:
                ui_node, _ = matches[0]
                break
            require(runner.poll() is None and not CANCELLED and time.monotonic() < deadline,
                    "Actual runner failed before UI; warm build/exit is not invented")
            time.sleep(0.05)
        require(digest(ui_path) == artifacts["cagents-ui"]["sha256"], "Runner changed UI artifact after prebuild")
        ui = self.cookie(ui_node, ui_path)
        self.ui_pid = ui["pid"]
        ownership = {"version": 1, "root": str(ROOT), "display": display,
                     "xauthority": {"path": str(auth), **file_identity(auth), "sha256": digest(auth)},
                     "server": server_cookie, "ui": ui}
        owned_ref = save(self.work / "ownership.json", ownership)
        build_ref = save(self.work / "builds.json", {"version": 1, **self.pin, "checkout": str(CHECKOUT),
            "source_files": self.source["source_files"], "ui": ui, "ui_sha256": artifacts["cagents-ui"]["sha256"],
            "matched_builds": builds, "original_family_settled": True})
        require(fixture_log_identity(self.fixture_log_fd, log) == self.fixture_log_ref,
                "Runner redirection replaced the original UI log")
        launch_ref = save(self.work / "launch.json", {"version": 1, "side": self.side,
            "scene": self.plan["scene"], "viewport": self.plan["viewport"], **self.pin,
            "fixture_log": fixture_log_identity(self.fixture_log_fd, log),
            "controller_sha256": CONTROLLER_SHA, "source_binding_sha256": SOURCE_SHA,
            "ownership_receipt_sha256": owned_ref["sha256"], "ui": ui, "server": server_cookie,
            "ui_sha256": artifacts["cagents-ui"]["sha256"], "display": display,
            "checkout": str(CHECKOUT), **self.source, "build_receipt": build_ref})
        self.controller_env = env  # Never fixture opt-in or private fixture XDG.
        self.controller_argv = [self.plan["tools"]["python3"]["path"], "-I", "-B", str(HERE / "controller.py"),
            "--expected-root", str(ROOT), "--receipt", owned_ref["path"], "--display", display,
            "--xauthority", str(auth), "--launch", launch_ref["path"], "--launch-sha256", launch_ref["sha256"]]
        self.event("actual-scene", ownership=owned_ref, launch=launch_ref, namespace=namespace.receipt(),
                   runner_pid=runner.pid, ui_pid=ui["pid"], daemon_started=False,
                   limit="Inert fixture only; no daemon/provider execution or natural-close UX")
        self.scene_started = True

    def inputs(self):
        ready_end = min(self.end, time.monotonic() + 60)
        for observation in range(32):
            # Successful empty listings are bounded startup observations. A
            # failed child is never retried, and no input has been admitted yet.
            require(time.monotonic() < ready_end, "Original window-readiness budget expired")
            record, _ = self.command(self.controller_argv + ["list"], "controller-list",
                                     min(20, ready_end - time.monotonic()), env=self.controller_env)
            listing = json.loads(read_bytes(record["stdout"], private=True), object_pairs_hook=pairs)
            windows = [w for w in listing["windows"] if w["expected_class"] and w["viewable"] and w["pid"] == self.ui_pid
                       and not w["override_redirect"]]
            require(len(windows) <= 1, "Ambiguous actual development window")
            if windows:
                break
            self.event("window-not-yet-mapped", successful_observation=observation, no_input=True)
            time.sleep(0.5)
        require(len(windows) == 1, "No unique actual development window; no guessed window/restart")
        window = str(windows[0]["window"])
        phases = json.loads(read_bytes(HERE / "phases.json"), object_pairs_hook=pairs)["scenes"][self.plan["scene"]][self.side]["phases"]
        self.event("manual-review-ready", actual_window=windows[0], next_phase=phases[0]["phase"],
                   protocol="capture; then phase with private original review path/hash; finish after all phases")
        phase_index, observations = 0, 0
        while True:
            self.check()
            print(json.dumps({"side": self.side, "next_phase": phases[phase_index]["phase"] if phase_index < len(phases) else None,
                              "work": str(self.work), "status": "WAITING_FOR_ACTUAL_ROOT_FRAME_REVIEW"}), flush=True)
            line = manual_line(self.end)
            action = json.loads(line, object_pairs_hook=pairs)
            if action == {"op": "capture"}:
                require(observations < 2, "Observation budget exhausted; no reset/retry")
                self.command(self.controller_argv + ["--window", window, "capture"], "controller-capture", 20,
                             env=self.controller_env)
                observations += 1
            elif action == {"op": "finish"}:
                require(phase_index == len(phases), "Incomplete fixed native flow; cannot finish early")
                self.event("observations-completed", phases=len(phases),
                           race_claim=False, logical_expiry_is_not_watchdog_timing=True,
                           widget_focus_not_inferred_from_x_focus=True)
                return
            else:
                require(set(action) == {"op", "phase", "review", "review_sha256"} and action["op"] == "phase"
                        and phase_index < len(phases) and action["phase"] == phases[phase_index]["phase"],
                        "Only the exact next fixed phase may be admitted")
                review = Path(action["review"])
                require(ROOT in review.parents and review.parent.resolve(strict=True) == review.parent,
                        "Root review must be an original private in-root receipt")
                pinned({"path": str(review), "sha256": action["review_sha256"]}, private=True)
                self.command(self.controller_argv + ["--window", window, "phase", "--phase", action["phase"],
                    "--review", str(review), "--review-sha256", action["review_sha256"]], "controller-phase", 20,
                    env=self.controller_env)
                phase_index += 1
                observations = 0

    def close(self):
        # Intentionally unbounded positive cleanup. Cancellation/deadlines cannot
        # abandon a family or turn an unknown original command into exit0.
        def blocked(kind, error):
            try:
                self.event("settlement-blocked", kind=kind, error=type(error).__name__ + ": " + str(error),
                           allocation_retained=True)
            except OSError:
                print("Settlement evidence write failed; original custody and C retained", file=sys.stderr, flush=True)
        snapshot = self.owner.settle_all(lambda: None, blocked)
        self.owner.close_pidfds()
        if self.fixture_log_fd is not None:
            require(not self.fixture_log_close_attempted,
                    "Original UI log close outcome is unknown; no close retry, allocation retained")
            try:
                self.fixture_log_raw = fixture_log_bytes(self.fixture_log_fd, self.fixture_log_ref)
                capture = self.work / "ui-log-final.bin"
                with os.fdopen(os.open(capture, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW | os.O_CLOEXEC,
                                       0o600), "wb") as output:
                    output.write(self.fixture_log_raw)
                    output.flush()
                    os.fsync(output.fileno())
                self.fixture_log_capture = {"path": str(capture), "bytes": len(self.fixture_log_raw),
                    "sha256": hashlib.sha256(self.fixture_log_raw).hexdigest()}
            except Exception as error:
                self.fixture_log_error = type(error).__name__ + ": " + str(error)
            finally:
                self.fixture_log_close_attempted = True
                try:
                    os.close(self.fixture_log_fd)
                except BaseException as error:
                    self.fixture_log_close_error = type(error).__name__ + ": " + str(error)
                    raise
                else:
                    self.fixture_log_fd, self.fixture_log_closed = None, True
                finally:
                    self.event("original-ui-log-snapshot", original=self.fixture_log_ref,
                        capture=self.fixture_log_capture, error=self.fixture_log_error,
                        descriptor_close_attempted=self.fixture_log_close_attempted,
                        descriptor_closed=self.fixture_log_closed, descriptor_close_error=self.fixture_log_close_error)
        require(self.fixture_log_error is None and self.fixture_log_close_error is None,
                "Original UI log snapshot failed; allocation retained: " + str(self.fixture_log_error))
        self.settled = True
        for record in self.commands:
            if record.get("original_exit") is None and "pid" in record:
                record["cleanup_exit"] = self.owner.direct[record["pid"]].wait(timeout=0)
            record.update(stdout_sha256=digest(Path(record["stdout"])), stderr_sha256=digest(Path(record["stderr"])))
        for record in self.services:
            record.update(stdout_sha256=digest(Path(record["stdout"])), stderr_sha256=digest(Path(record["stderr"])))
        self.event("positive-family-closure", custody=snapshot, commands=self.commands,
                   services=self.services, original_waits=[{"pid": pid, "exit": p.wait(timeout=0)}
                                                        for pid, p in self.owner.direct.items()],
                   pidfds_closed_in_original_records=True,
                   fixture_log={"original": self.fixture_log_ref, "capture": self.fixture_log_capture,
                                "error": self.fixture_log_error, "descriptor_closed": self.fixture_log_closed},
                   UI_exit="planned owned SIGTERM/SIGKILL cleanup, not natural UI-close evidence")

    def fixture_receipts(self):
        require(self.scene_started and self.settled, "Only the original settled scene log is interpreted")
        runner = next(s for s in self.services if s["role"] == "owned-development-runner")
        require(self.fixture_log_closed and self.fixture_log_fd is None
                and self.fixture_log_error is None and self.fixture_log_capture is not None,
                "Original UI log was not positively captured/closed")
        events = fixture_log_events(self.fixture_log_raw, self.plan["scene"])
        specification = json.loads(read_bytes(HERE / "phases.json"), object_pairs_hook=pairs)["scenes"][self.plan["scene"]][self.side]
        completions = [e for e in events if e.get("event") == "actual_reducer_completion"]
        key = "source_oracle_satisfied" if self.side == "after" else "expected_before_consequence_observed"
        require(completions and all(e.get(key) is True for e in completions),
                "Actual fixture source oracle absent/failed")
        require([e["kind"] for e in completions] == specification["required_completion_kinds"],
                "Missing, duplicate or unexpected original completion")
        states = [e for e in events if e.get("event") == "scene_state"]
        require(states and states[-1].get("phase") in specification["terminal_states"],
                "Incomplete source-specific native flow")
        if specification["unavailable_control_expected"]:
            require(states[-1].get("native_control_gap") is not None
                    and completions[-1]["after"]["accounts_table_row"] is False,
                    "Before missing-control consequence was not observed")
        return self.event("original-fixture-log", events=events,
            original_fixture_log=self.fixture_log_ref, retained_original_bytes=self.fixture_log_capture,
            original_log_descriptor_closed=self.fixture_log_closed,
            original_runner_stdout={"path": runner["stdout"], "sha256": digest(Path(runner["stdout"]))},
            side=self.side, scene=self.plan["scene"], viewport=self.plan["viewport"],
            observed_source_oracle=key, native_UX_result="PENDING_INDEPENDENT_REVIEW",
            limit="Before injected old-parser/callback; After actual held collectors. No provider/transport/login/watchdog or startup-readiness evidence.")


def file_identity(path):
    info = Path(path).lstat()
    return {"device": info.st_dev, "inode": info.st_ino}


def read_bytes_prefix(path, length):
    with open(path, "rb") as source:
        return source.read(length)


def live_output(path, limit):
    # Explicitly a partial live-log observation, not an immutable artifact.
    fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_CLOEXEC)
    try:
        info = os.fstat(fd)
        require(stat.S_ISREG(info.st_mode) and info.st_uid == os.getuid() and info.st_nlink == 1
                and stat.S_IMODE(info.st_mode) == 0o600, "Unsafe original live log")
        raw = os.read(fd, limit + 1)
        require(len(raw) <= limit, "Live log exceeds bound")
        return raw
    finally:
        os.close(fd)


def manual_line(deadline):
    data = bytearray()
    while b"\n" not in data:
        require(not CANCELLED and time.monotonic() < deadline, "Original manual-review deadline/cancellation")
        ready, _, _ = select.select([sys.stdin], [], [], min(0.1, max(0, deadline - time.monotonic())))
        if not ready:
            continue
        chunk = os.read(sys.stdin.fileno(), 4097 - len(data))
        require(chunk, "Original input stream closed")
        data.extend(chunk)
        require(len(data) <= 4096, "Original input instruction exceeds4KiB")
    require(data.count(b"\n") == 1 and data.endswith(b"\n"), "One complete original instruction required")
    return bytes(data)


def initial_registry(plan):
    initial = plan["initial_checkout"]
    # Filesystem schema/hash/identity only. No initial Git/status or Cargo read.
    record = pinned(initial["registry"], private=True)
    require(set(record) == {"version", "owner", "uid", "checkout", "common_git_dir", "common_identity",
                           "checkout_git_dir", "checkout_git_dir_identity", "head", "tree"}
            and record["version"] == 1 and record["owner"] == OWNER and record["uid"] == os.getuid()
            and record["checkout"] == str(CHECKOUT) and record["common_git_dir"] == str(COMMON)
            and record["head"] == initial["head"] and record["tree"] == initial["tree"], "Original slot registry mismatch")
    expected_marker = COMMON / "validation-checkouts-v1" / (hashlib.sha256(str(CHECKOUT).encode()).hexdigest() + ".json")
    require(initial["registry"]["path"] == str(expected_marker)
            and identity(CHECKOUT) == initial["checkout_identity"]
            and record["common_identity"] == [COMMON.stat().st_dev, COMMON.stat().st_ino]
            and Path(record["checkout_git_dir"]).parent == COMMON / "worktrees"
            and record["checkout_git_dir_identity"] == [identity(record["checkout_git_dir"])["device"],
                                                      identity(record["checkout_git_dir"])["inode"]],
            "Original slot/common/admin identity changed")
    pinned(initial["accepted_prior_return"], owned_input=True)
    require(initial["accepted_prior_return"] in plan["reviewed_inputs"], "Accepted prior return must be repeatedly guarded")
    return initial["head"]


def namespace_owner(namespace, cases):
    """The exact original scene owns its descriptors, including partial startup."""
    matches = [case for case in cases if case.side == namespace.variant
               and case.plan["scene"] == namespace.scene and case.plan["viewport"] == namespace.viewport]
    require(len(matches) == 1 and matches[0].settled,
            "Original namespace scene owner missing or not positively settled")
    return matches[0].owner, {"namespace_variant":namespace.variant,"owner_side":matches[0].side,
                              "scene":namespace.scene,"viewport":namespace.viewport,
                              "reason":"exact-original-scene-owner-no-adoption"}


def main():
    global C_LOCK_FD
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--allocation", required=True)
    parser.add_argument("--allocation-sha256", required=True)
    args = parser.parse_args()
    plan = admission(args)
    plan["adapter_sha256"] = digest(Path(__file__).resolve())
    root_policy = plan["capture_root"]
    if root_policy["previous_receipt"] is None:
        ROOT.parent.mkdir(mode=0o700, exist_ok=True)
        require(identity(ROOT.parent), "Unsafe artifact parent")
        ROOT.mkdir(mode=0o700)  # First collection only; no existing-root adoption.
    else:
        require(root_policy["previous_receipt"] in plan["reviewed_inputs"], "Prior native return must remain repeatedly pinned")
        prior = pinned(root_policy["previous_receipt"], private=True)
        require(prior["root"] == str(ROOT) and prior["root_identity"] == root_policy["identity"]
                and prior["allocation_releasable"] is True and prior["namespaces_closed"] is True,
                "Prior original adapter return does not authorize reopening its artifact directory")
        require(identity(ROOT) == root_policy["identity"], "Earlier artifact directory replaced")
    plan["_root_identity"] = identity(ROOT)
    require(stat.S_IMODE(ROOT.stat().st_mode) == 0o700, "Capture root must be0700")
    captures = ROOT / "captures"
    if not captures.exists():
        captures.mkdir(mode=0o700)
    require(identity(captures) and stat.S_IMODE(captures.stat().st_mode) == 0o700, "Unsafe capture directory")
    guard(plan)
    # Existing lock inode is supplied by the scheduler. Never create another C.
    lock_fd = os.open(LOCK, os.O_RDWR | os.O_NOFOLLOW | os.O_CLOEXEC)
    lock_info = os.fstat(lock_fd)
    require(stat.S_ISREG(lock_info.st_mode) and lock_info.st_uid == os.getuid() and lock_info.st_nlink == 1
            and stat.S_IMODE(lock_info.st_mode) == 0o644
            and file_identity(LOCK) == plan["lock_identity"] == {"device": lock_info.st_dev, "inode": lock_info.st_ino},
            "Original C lock changed")
    fcntl.flock(lock_fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
    C_LOCK_FD = lock_fd
    require(identity(TARGET) == plan["target_identity"], "Original canonical target changed")
    engine = runpy.run_path(str(Path(plan["control"]["repository"]) / "scripts/qualification/process_custody.py"))
    namespace_module = runpy.run_path(str(HERE / "namespace.py"))
    cases, helpers, namespaces = [], [], []
    namespace_closures = []
    error, namespaces_closed = None, False
    clean_verified = False
    try:
        expected = initial_registry(plan)
        for side in ("before", "after"):
            # A prior side's clean result cannot cover any subsequent admission,
            # build, native observation or fallible cleanup/evidence operation.
            clean_verified = False
            case = Case(plan, side, engine)
            cases.append(case)
            original_failure = None
            try:
                case.admit(expected)  # First initial-worktree Git validation.
                if "_context" not in plan:
                    plan["_context"] = build_context(plan)  # Never reset after transition.
                builds, artifacts = case.builds()
                current = namespace_module["SceneNamespace"](plan["scene"], plan["viewport"], side)
                namespaces.append(current)
                case.start_native(current, builds, artifacts)
                case.inputs()
            except BaseException as failure:
                original_failure = failure
            finally:
                case.close()
            # Actual helper is reused for final strict ignored/untracked clean
            # validation only after genuine scene settlement, not a reset/clean.
            clean_verified = False
            checker = Case(plan, side, engine, helper_only=True)
            helpers.append(checker)
            try:
                checker.admit(case.pin["head"])
            finally:
                checker.close()
            clean_verified = True
            # This helper-only owner NEVER grants namespace authority. Both
            # owners now have genuine positive settlement and original waits.
            if case.scene_started:
                case.fixture_receipts()
            if original_failure is not None:
                raise original_failure
            expected = case.pin["head"]
            # The original scene authorizes ONLY its own namespace close.
            # No archive/reset/reuse: After later creates a different fixed root.
            require(case.scene_started and case.settled, "No genuine original scene owner")
            owner, binding = namespace_owner(current, cases)
            current.close_original_descriptors(owner)
            namespace_closures.append(binding)
            namespaces_closed = all(n.descriptors_closed for n in namespace_module["ALL_NAMESPACES"])
    except BaseException as failure:
        error = {"type": type(failure).__name__, "message": str(failure)}
        # A failed observation may still return C if every actual resource and
        # source postcondition is positively known. It never becomes a pass.
        if cases and all(c.settled for c in cases + helpers):
            try:
                for namespace in reversed(namespace_module["ALL_NAMESPACES"]):
                    if not namespace.descriptors_closed and not namespace.close_attempted:
                        owner, binding = namespace_owner(namespace, cases)
                        namespace.close_original_descriptors(owner)
                        namespace_closures.append(binding)
                namespaces_closed = all(n.descriptors_closed for n in namespace_module["ALL_NAMESPACES"])
            except BaseException as closure_failure:
                error["namespace_closure_error"] = type(closure_failure).__name__ + ": " + str(closure_failure)
        # Partial/pre-scene objects and unknown closes retain their original FDs.
    finally:
        families_closed = all(case.settled for case in cases + helpers) and bool(cases)
        allocation_releasable = families_closed and namespaces_closed and clean_verified
        result = {"root": str(ROOT), "root_identity": plan["_root_identity"], "scene": plan["scene"], "viewport": plan["viewport"],
                  "adapter_sha256": plan["adapter_sha256"], "plan_sha256": PLAN_SHA256, "pair": plan["pair"],
                  "state": "FAILED_WITH_POSITIVE_CLOSURE" if error and allocation_releasable else
                           "NATIVE_OBSERVATIONS_COMPLETE_PENDING_RAW_UX" if allocation_releasable else "FAILED_OR_HOLD",
                  "error": error, "allocation_releasable": allocation_releasable,
                  "namespaces_closed": namespaces_closed, "final_helper_clean_verified": clean_verified,
                  "namespace_close_authorities": namespace_closures,
                  "namespaces": [n.receipt() for n in namespace_module["ALL_NAMESPACES"]],
                  "cases": [{"side": c.side, "scene_started": c.scene_started, "settled": c.settled,
                             "records": c.records} for c in cases],
                  "final_helper_owners": [{"side": c.side, "settled": c.settled, "records": c.records} for c in helpers],
                  "grant": plan["_grant"],
                  "qualification": "No automatic PASS, screenshot review, responsiveness or same-batch race claim"}
        save(ROOT / (plan["scene"] + "-" + plan["viewport"] + "-return.json"), result)
        if allocation_releasable:
            fcntl.flock(lock_fd, fcntl.LOCK_UN)
            os.close(lock_fd)
            C_LOCK_FD = None
        else:
            # Unknown custody/descriptors/cleanliness retains C. The original
            # wrapper must remain alive, not implicitly release flock on exit.
            print("HOLD: original allocation retained; inspect original evidence", file=sys.stderr, flush=True)
            while True:
                time.sleep(1)
    return 1 if error else 0


def cancelled(_number, _frame):
    global CANCELLED
    CANCELLED = True


if __name__ == "__main__":
    signal.signal(signal.SIGTERM, cancelled)
    signal.signal(signal.SIGINT, cancelled)
    try:
        raise SystemExit(main())
    except Exception as error:
        print(json.dumps({"state": "INVALID_OR_FAILED", "error": type(error).__name__ + ": " + str(error),
                          "no_execution_permission_created": True}), file=sys.stderr)
        if C_LOCK_FD is not None:
            print("HOLD: failure after actual C acquisition; original lock retained", file=sys.stderr, flush=True)
            while True:
                time.sleep(1)
        raise SystemExit(2)
