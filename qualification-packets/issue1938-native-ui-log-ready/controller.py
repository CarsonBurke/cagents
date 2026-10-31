#!/usr/bin/env python3
"""Fixed PR1775 Automations controller for one private Xvfb fixture.

This file has only been parsed/compiled as source. Runtime use requires separate
review and authorization. It never changes the environment or starts programs.
"""

import argparse
import ctypes as C
import hashlib
import json
import os
from pathlib import Path
import re
import select
import socket
import stat
import struct
import sys
import uuid


APP_ID = "dev.cagents.Cagents.Development"
ALLOWED_ROOT = Path('/tmp/coord16-1938-native-ui-log-corrected-1f1e78ab/owned-native')
MAX_WINDOWS = 4096
MAX_DEPTH = 16

FORBIDDEN_NAMES = ('CAGENTS_DEVELOPMENT_ACCOUNT_FIXTURE', 'CAGENTS_DEVELOPMENT_AUTOMATION_FIXTURE', 'WAYLAND_DISPLAY', 'WAYLAND_SOCKET', 'CAGENTS_DEVELOPMENT_NATIVE_IDENTITY_FIXTURE', 'CAGENTS_CHAT_STAGE_OUTPUT', 'CAGENTS_UI_LAUNCH_HANDSHAKE', 'CAGENTS_UI_LAUNCH_RECOVERY_PROGRESS', 'CAGENTS_UI_UPDATE_GUARD_FD', 'CAGENTS_DAEMON_LAUNCH_FENCE_FD', 'CAGENTS_DEVELOPMENT_DAEMON_RECOVERY_FIXTURE', 'CAGENTS_LATENCY_OUTPUT', 'CAGENTS_INSTALL_RESULT_FD', 'CAGENTS_INSTALL_OWNER_CLI', 'CAGENTS_ARCHIVE', 'CAGENTS_CHECKSUMS', 'CAGENTS_REPOSITORY', 'XDG_ACTIVATION_TOKEN', 'DESKTOP_STARTUP_ID', 'GIT_DIR', 'GIT_WORK_TREE', 'GIT_COMMON_DIR', 'GIT_INDEX_FILE', 'GIT_CONFIG', 'GIT_CONFIG_COUNT', 'GIT_CONFIG_PARAMETERS', 'GIT_CONFIG_SYSTEM', 'GIT_CONFIG_GLOBAL', 'GIT_CONFIG_NOSYSTEM', 'GIT_OBJECT_DIRECTORY', 'GIT_ALTERNATE_OBJECT_DIRECTORIES', 'GIT_CEILING_DIRECTORIES', 'GIT_DISCOVERY_ACROSS_FILESYSTEM', 'RUSTC_WRAPPER', 'RUSTC_WORKSPACE_WRAPPER', 'RUSTFLAGS', 'CARGO_ENCODED_RUSTFLAGS')
FORBIDDEN_PREFIXES = ('CARGO_PROFILE_', 'GIT_CONFIG_KEY_', 'GIT_CONFIG_VALUE_')



SOURCE_BINDING = Path(__file__).with_name("source-binding.json")
SOURCE_BINDING_SHA256 = '75843169bba188989984e22d72471100a178a2f5d9c8ac4b7bd164616bdf0e77'
PHASE_PLAN = Path(__file__).with_name("phases.json")
PHASE_PLAN_SHA256 = '2a09badabaabd2aa377eaefb0e4b5d029926498cf32d49f723df273e06aca2e1'

class Refused(Exception):
    pass


def require(condition, message):
    if not condition:
        raise Refused(message)


def number(value, name, minimum=0):
    require(type(value) is int and value >= minimum, f"Invalid {name}")
    return value


def exact_keys(value, keys, name, optional=()):
    require(type(value) is dict, f"Invalid {name}")
    require(set(keys) <= set(value) <= set(keys) | set(optional), f"Invalid {name} fields")


def no_duplicate_keys(pairs):
    result = {}
    for key, value in pairs:
        require(key not in result, "Receipt repeats a field")
        result[key] = value
    return result


def below_root(value, root):
    require(type(value) is str, "Expected an explicit absolute path")
    path = Path(value)
    require(path.is_absolute() and str(path) == value, "Path must be normalized and absolute")
    require(".." not in path.parts and path != root, "Invalid owned path")
    require(path.is_relative_to(root), "Path is outside the expected fixture root")
    current = root
    for part in path.relative_to(root).parts:
        current /= part
        info = current.lstat()
        require(not stat.S_ISLNK(info.st_mode), "Owned path contains a symlink")
        if current != path:
            require(stat.S_ISDIR(info.st_mode), "Owned path has a non-directory parent")
            require(info.st_uid == os.getuid() and info.st_mode & 0o022 == 0,
                    "Owned parent must not be writable by another uid")
    return path


def private_bytes(path, maximum):
    fd = os.open(path, os.O_RDONLY | os.O_CLOEXEC | os.O_NOFOLLOW)
    try:
        info = os.fstat(fd)
        require(stat.S_ISREG(info.st_mode) and info.st_nlink == 1,
                "Expected a single-link regular private file")
        require(info.st_uid == os.getuid() and info.st_mode & 0o077 == 0,
                "Expected a file private to this uid")
        require(0 < info.st_size <= maximum, "Private file has an invalid size")
        data = os.read(fd, maximum + 1)
        require(len(data) == info.st_size and len(data) <= maximum,
                "Private file changed or exceeds its size limit")
        after = os.fstat(fd)
        identity = lambda entry: (entry.st_dev, entry.st_ino, entry.st_size,
                                  entry.st_mtime_ns, entry.st_ctime_ns,
                                  entry.st_mode, entry.st_uid, entry.st_nlink)
        require(identity(after) == identity(info), "Private file changed while being read")
        return data, info
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
            event = json.loads(line[len(b"ACCOUNT_NATIVE "):], object_pairs_hook=no_duplicate_keys)
            require(type(event) is dict and type(event.get("event")) is str, "Invalid original fixture event")
            require(event.get("scene", scene) == scene, "Original scene changed")
            require(event["event"] not in ("fixture_error", "step_refused"), "Original fixture refused/stopped")
            events.append(event)
            require(len(events) <= 4096, "Original fixture event bound exceeded")
    require(events, "No original fixture evidence")
    return events


def read_fixture_log(owned, reference):
    require(type(reference) is dict and "path" in reference, "Original UI log reference absent")
    path = below_root(reference["path"], owned.root)
    fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_CLOEXEC)
    try:
        return fixture_log_bytes(fd, reference)
    finally:
        os.close(fd)


def process_cookie(process, boot_id, server=False):
    fields = {"pid", "start_ticks", "uid", "executable"}
    if server:
        fields |= {"boot_id", "socket"}
    exact_keys(process, fields, "process cookie")
    pid = number(process["pid"], "process pid", 1)
    number(process["start_ticks"], "process start ticks", 1)
    number(process["uid"], "process uid")
    require(process["uid"] == os.getuid(), "Process receipt uid does not match")
    executable = process["executable"]
    require(type(executable) is str and Path(executable).is_absolute(), "Invalid executable")
    require(Path(executable).name == ("Xvfb" if server else "cagents-ui"),
            "Unexpected executable name")
    require(Path(f"/proc/{pid}").stat().st_uid == os.getuid(), "Process uid changed")
    text = Path(f"/proc/{pid}/stat").read_text()
    end = text.rfind(")")
    require(end > 0, "Invalid process metadata")
    values = text[end + 2:].split()
    require(len(values) > 19 and values[0] not in ("Z", "X"), "Process is not alive")
    require(int(values[19]) == process["start_ticks"], "Process start cookie changed")
    require(os.readlink(f"/proc/{pid}/exe") == executable, "Process executable changed")
    active_executable = os.stat(f"/proc/{pid}/exe")
    named_executable = os.stat(executable)
    require((active_executable.st_dev, active_executable.st_ino) ==
            (named_executable.st_dev, named_executable.st_ino), "Executable was replaced")
    if server:
        require(process["boot_id"] == boot_id, "Server boot cookie changed")


class Ownership:
    def __init__(self, args):
        self.pidfds = []
        self.root = Path(args.expected_root)
        require(self.root == ALLOWED_ROOT and str(self.root) == args.expected_root,
                "Only the explicitly reviewed fixture root is accepted")
        root_info = self.root.lstat()
        require(stat.S_ISDIR(root_info.st_mode) and not stat.S_ISLNK(root_info.st_mode),
                "Fixture root must be a real directory")
        require(root_info.st_uid == os.getuid() and root_info.st_mode & 0o077 == 0,
                "Fixture root must be private to this uid")
        self.root_identity = (root_info.st_dev, root_info.st_ino)
        self.receipt_path = below_root(args.receipt, self.root)
        data, self.receipt_info = private_bytes(self.receipt_path, 16384)
        self.receipt_digest = hashlib.sha256(data).hexdigest()
        receipt = json.loads(data, object_pairs_hook=no_duplicate_keys)
        exact_keys(receipt, {"version", "root", "display", "xauthority", "server"},
                   "receipt", optional={"ui"})
        require(type(receipt["version"]) is int and receipt["version"] == 1,
                "Unsupported ownership receipt version")
        require(receipt["root"] == str(self.root), "Receipt root differs from explicit root")
        require(type(args.display) is str and re.fullmatch(r":[1-9][0-9]{0,4}", args.display),
                "Display must be an explicit local nonzero display number")
        require(int(args.display[1:]) <= 65535, "Display number is too large")
        self.display = args.display
        require(receipt["display"] == self.display, "Receipt display differs from explicit display")
        exact_keys(receipt["xauthority"], {"path", "device", "inode", "sha256"}, "Xauthority receipt")
        self.auth = receipt["xauthority"]
        require(self.auth["path"] == args.xauthority, "Xauthority differs from explicit path")
        self.auth_path = below_root(args.xauthority, self.root)
        number(self.auth["device"], "Xauthority device")
        number(self.auth["inode"], "Xauthority inode", 1)
        require(type(self.auth["sha256"]) is str and
                re.fullmatch(r"[0-9a-f]{64}", self.auth["sha256"]), "Invalid Xauthority digest")
        # These two variables are checked, never used as defaults or modified.
        require(os.environ.get("DISPLAY") == self.display,
                "DISPLAY must already equal the explicitly supplied display")
        require(os.environ.get("XAUTHORITY") == str(self.auth_path),
                "XAUTHORITY must already equal the explicitly supplied private file")
        self.server = receipt["server"]
        self.ui = receipt.get("ui")
        require(type(self.server) is dict, "Missing server process cookie")
        self.boot_id = Path("/proc/sys/kernel/random/boot_id").read_text().strip()
        uuid.UUID(self.boot_id)
        self.server_executable = below_root(self.server.get("executable"), self.root)
        self.socket_path = Path(f"/tmp/.X11-unix/X{self.display[1:]}")
        exact_keys(self.server.get("socket"), {"path", "device", "inode"}, "server socket")
        require(self.server["socket"]["path"] == str(self.socket_path), "Unexpected server socket path")
        number(self.server["socket"]["device"], "socket device")
        number(self.server["socket"]["inode"], "socket inode", 1)
        self.check(need_ui=self.ui is not None)
        require(hasattr(os, "pidfd_open"), "A Linux pidfd is required")
        try:
            self.pidfds.append(("server", os.pidfd_open(self.server["pid"], 0)))
            if self.ui is not None:
                self.pidfds.append(("ui", os.pidfd_open(self.ui["pid"], 0)))
            self.check(need_ui=self.ui is not None)
        except BaseException:
            self.close()
            raise

    def close(self):
        for _role, fd in self.pidfds:
            os.close(fd)
        self.pidfds.clear()

    def check(self, need_ui=False):
        root_info = self.root.lstat()
        require((root_info.st_dev, root_info.st_ino) == self.root_identity and
                stat.S_ISDIR(root_info.st_mode) and root_info.st_mode & 0o077 == 0 and
                root_info.st_uid == os.getuid(), "Fixture root identity changed")
        receipt_data, receipt_info = private_bytes(below_root(str(self.receipt_path), self.root), 16384)
        require((receipt_info.st_dev, receipt_info.st_ino) ==
                (self.receipt_info.st_dev, self.receipt_info.st_ino) and
                hashlib.sha256(receipt_data).hexdigest() == self.receipt_digest,
                "Ownership receipt changed during this command")
        process_cookie(self.server, self.boot_id, server=True)
        below_root(str(self.server_executable), self.root)
        if need_ui and self.ui is not None:
            process_cookie(self.ui, self.boot_id)
        require(not need_ui or self.ui is not None, "A verified UI process cookie is required")
        for role, fd in self.pidfds:
            if role == "ui" and not need_ui:
                continue
            poller = select.poll()
            poller.register(fd, select.POLLIN)
            require(not poller.poll(0), "A pinned process exited")
        auth_data, auth_info = private_bytes(below_root(str(self.auth_path), self.root), 8192)
        require((auth_info.st_dev, auth_info.st_ino) ==
                (self.auth["device"], self.auth["inode"]), "Xauthority file identity changed")
        require(hashlib.sha256(auth_data).hexdigest() == self.auth["sha256"],
                "Xauthority file digest changed")
        socket_directory = self.socket_path.parent.lstat()
        require(stat.S_ISDIR(socket_directory.st_mode) and not stat.S_ISLNK(socket_directory.st_mode),
                "The X11 socket directory must be a real directory")
        socket_info = self.socket_path.lstat()
        require(stat.S_ISSOCK(socket_info.st_mode) and socket_info.st_uid == os.getuid(),
                "Expected the owned Xvfb Unix socket")
        require((socket_info.st_dev, socket_info.st_ino) ==
                (self.server["socket"]["device"], self.server["socket"]["inode"]),
                "Server socket identity changed")
        require(os.environ.get("DISPLAY") == self.display and
                os.environ.get("XAUTHORITY") == str(self.auth_path), "Display environment changed")


class WindowAttributes(C.Structure):
    _fields_ = [
        ("x", C.c_int), ("y", C.c_int), ("width", C.c_int), ("height", C.c_int),
        ("border_width", C.c_int), ("depth", C.c_int), ("visual", C.c_void_p),
        ("root", C.c_ulong), ("window_class", C.c_int), ("bit_gravity", C.c_int),
        ("win_gravity", C.c_int), ("backing_store", C.c_int),
        ("backing_planes", C.c_ulong), ("backing_pixel", C.c_ulong),
        ("save_under", C.c_int), ("colormap", C.c_ulong), ("map_installed", C.c_int),
        ("map_state", C.c_int), ("all_event_masks", C.c_long),
        ("your_event_mask", C.c_long), ("do_not_propagate_mask", C.c_long),
        ("override_redirect", C.c_int), ("screen", C.c_void_p),
    ]


class ErrorEvent(C.Structure):
    _fields_ = [("type", C.c_int), ("display", C.c_void_p),
                ("resourceid", C.c_ulong), ("serial", C.c_ulong),
                ("error_code", C.c_ubyte), ("request_code", C.c_ubyte),
                ("minor_code", C.c_ubyte)]


class X11:
    def __init__(self, owned):
        self.owned = owned
        self.display = None
        self.errors = []
        require(sys.platform == "linux" and C.sizeof(C.c_void_p) == 8,
                "This reviewed controller requires 64-bit Linux")
        owned.check()
        self.lib = C.CDLL("/lib64/libX11.so.6")
        self.xtst = None
        P = C.c_void_p
        U = C.c_ulong
        I = C.c_int
        self.bind("XOpenDisplay", P, [C.c_char_p])
        self.bind("XCloseDisplay", I, [P])
        self.bind("XConnectionNumber", I, [P])
        self.bind("XDefaultRootWindow", U, [P])
        self.bind("XSync", I, [P, I])
        self.bind("XFree", I, [P])
        self.bind("XInternAtom", U, [P, C.c_char_p, I])
        self.bind("XGetWindowProperty", I, [P, U, U, C.c_long, C.c_long, I, U,
                  C.POINTER(U), C.POINTER(I), C.POINTER(U), C.POINTER(U), C.POINTER(P)])
        self.bind("XGetWindowAttributes", I, [P, U, C.POINTER(WindowAttributes)])
        self.bind("XQueryTree", I, [P, U, C.POINTER(U), C.POINTER(U),
                  C.POINTER(C.POINTER(U)), C.POINTER(C.c_uint)])
        self.bind("XTranslateCoordinates", I, [P, U, U, I, I, C.POINTER(I), C.POINTER(I), C.POINTER(U)])
        self.bind("XMoveResizeWindow", I, [P, U, I, I, C.c_uint, C.c_uint])
        self.bind("XRaiseWindow", I, [P, U])
        self.bind("XSetInputFocus", I, [P, U, I, U])
        self.bind("XGetInputFocus", I, [P, C.POINTER(U), C.POINTER(I)])
        self.bind("XStringToKeysym", U, [C.c_char_p])
        self.bind("XKeysymToKeycode", C.c_ubyte, [P, U])
        self.bind("XQueryKeymap", I, [P, C.POINTER(C.c_char)])
        self.bind("XQueryPointer", I, [P, U, C.POINTER(U), C.POINTER(U),
                  C.POINTER(I), C.POINTER(I), C.POINTER(I), C.POINTER(I), C.POINTER(C.c_uint)])
        self.error_callback_type = C.CFUNCTYPE(I, P, C.POINTER(ErrorEvent))
        self.bind("XSetErrorHandler", P, [P])
        self.callback = self.error_callback_type(self._error)
        self.previous_handler = self.lib.XSetErrorHandler(C.cast(self.callback, P))
        try:
            self.display = self.lib.XOpenDisplay(owned.display.encode("ascii"))
            require(bool(self.display), "Cannot open the explicit owned display")
            self.peer_check()
            owned.check()
            self.root = self.lib.XDefaultRootWindow(self.display)
        except BaseException:
            self.close()
            raise

    def bind(self, name, result, arguments, library=None):
        function = getattr(library or self.lib, name)
        function.restype = result
        function.argtypes = arguments

    def _error(self, _display, event):
        self.errors.append(int(event.contents.error_code))
        return 0

    def close(self):
        if self.display:
            self.lib.XCloseDisplay(self.display)
            self.display = None
        if hasattr(self, "previous_handler"):
            self.lib.XSetErrorHandler(self.previous_handler)
            del self.previous_handler

    def peer_check(self):
        fd = self.lib.XConnectionNumber(self.display)
        with socket.socket(fileno=os.dup(fd)) as connection:
            pid, uid, _gid = struct.unpack("3i", connection.getsockopt(socket.SOL_SOCKET,
                                            socket.SO_PEERCRED, struct.calcsize("3i")))
        require(pid == self.owned.server["pid"] and uid == os.getuid(),
                "X11 connection peer is not the pinned Xvfb process")

    def guard(self, need_ui=False):
        self.owned.check(need_ui=need_ui)
        self.peer_check()

    def sync(self):
        self.lib.XSync(self.display, 0)
        if self.errors:
            self.errors.clear()
            raise Refused("X11 rejected a request; no automatic retry")

    def attributes(self, window):
        result = WindowAttributes()
        status = self.lib.XGetWindowAttributes(self.display, window, C.byref(result))
        self.sync()
        require(status != 0, "Window no longer exists")
        return result

    def tree(self, window):
        root, parent = C.c_ulong(), C.c_ulong()
        children = C.POINTER(C.c_ulong)()
        count = C.c_uint()
        status = self.lib.XQueryTree(self.display, window, C.byref(root), C.byref(parent),
                                    C.byref(children), C.byref(count))
        try:
            self.sync()
            require(status != 0 and count.value <= MAX_WINDOWS, "Invalid or oversized window tree")
            return parent.value, [int(children[i]) for i in range(count.value)]
        finally:
            if children:
                self.lib.XFree(children)

    def property(self, window, name, property_type, expected_format):
        atom = self.lib.XInternAtom(self.display, name.encode("ascii"), 1)
        if not atom:
            return None
        actual, count, remaining = C.c_ulong(), C.c_ulong(), C.c_ulong()
        actual_format = C.c_int()
        data = C.c_void_p()
        status = self.lib.XGetWindowProperty(self.display, window, atom, 0, 256, 0,
                    property_type, C.byref(actual), C.byref(actual_format), C.byref(count),
                    C.byref(remaining), C.byref(data))
        try:
            self.sync()
            require(status == 0 and remaining.value == 0, "Invalid or oversized window property")
            if actual.value == 0:
                return None
            require(actual.value == property_type and actual_format.value == expected_format,
                    "Unexpected window property format")
            if expected_format == 8:
                return C.string_at(data, count.value)
            require(expected_format == 32, "Unsupported property format")
            values = C.cast(data, C.POINTER(C.c_ulong))
            return [int(values[i] & 0xffffffff) for i in range(count.value)]
        finally:
            if data:
                self.lib.XFree(data)

    def metadata(self, window):
        attributes = self.attributes(window)
        wm_class = self.property(window, "WM_CLASS", 31, 8)  # XA_STRING
        classes = [] if wm_class is None else wm_class.rstrip(b"\0").decode("utf-8", "replace").split("\0")
        process = self.property(window, "_NET_WM_PID", 6, 32)  # XA_CARDINAL
        pid = process[0] if process is not None and len(process) == 1 else None
        x, y, child = C.c_int(), C.c_int(), C.c_ulong()
        require(self.lib.XTranslateCoordinates(self.display, window, self.root, 0, 0,
                C.byref(x), C.byref(y), C.byref(child)) != 0, "Cannot locate window")
        self.sync()
        return {"window": hex(window), "pid": pid, "wm_class": classes,
                "geometry": {"x": x.value, "y": y.value, "width": attributes.width,
                             "height": attributes.height},
                "viewable": attributes.map_state == 2,
                "override_redirect": bool(attributes.override_redirect),
                "expected_class": classes == ["dev.cagents.Cagents.Development", "dev.cagents.Cagents.Development"]}

    def listing(self):
        self.guard()
        result, pending, seen = [], [(self.root, 0)], set()
        while pending:
            window, depth = pending.pop()
            require(window not in seen and len(seen) < MAX_WINDOWS and depth <= MAX_DEPTH,
                    "Window tree exceeded the review bounds")
            seen.add(window)
            if window != self.root:
                result.append(self.metadata(window))
            _, children = self.tree(window)
            pending.extend((child, depth + 1) for child in children)
        self.guard()
        return {"display": self.owned.display, "windows": result}

    def selected(self, window):
        self.guard(need_ui=True)
        require(window is not None and window != self.root, "An explicit fixture window is required")
        metadata = self.metadata(window)
        require(metadata["expected_class"] and metadata["pid"] == self.owned.ui["pid"],
                "Window does not match the pinned development application")
        require(metadata["viewable"], "Fixture window is not viewable")
        require(not metadata["override_redirect"], "Expected an ordinary non-override-redirect window")
        geometry = metadata["geometry"]
        root = self.attributes(self.root)
        require(0 <= geometry["x"] < root.width and 0 <= geometry["y"] < root.height and
                0 < geometry["width"] <= root.width - geometry["x"] and
                0 < geometry["height"] <= root.height - geometry["y"],
                "Fixture window is outside the owned root viewport")
        return metadata

    def descendant(self, candidate, ancestor):
        for _ in range(MAX_DEPTH + 1):
            if candidate == ancestor:
                return True
            if candidate in (0, self.root):
                return False
            candidate, _ = self.tree(candidate)
        raise Refused("Focus/window ancestry exceeded the review bounds")

    def focus_check(self, window):
        focused, revert = C.c_ulong(), C.c_int()
        self.lib.XGetInputFocus(self.display, C.byref(focused), C.byref(revert))
        self.sync()
        require(self.descendant(focused.value, window), "The selected fixture window does not own keyboard focus")

    def focus(self, window):
        self.selected(window)
        self.lib.XRaiseWindow(self.display, window)
        self.lib.XSetInputFocus(self.display, window, 2, 0)  # RevertToParent, CurrentTime
        self.sync()
        result = self.selected(window)
        self.focus_check(window)
        return result

    def resize_focus(self, window, x, y, width, height):
        self.selected(window)
        root = self.attributes(self.root)
        require(0 <= x and 0 <= y and 1 <= width <= root.width - x and
                1 <= height <= root.height - y, "Requested geometry exceeds the owned viewport")
        self.lib.XMoveResizeWindow(self.display, window, x, y, width, height)
        self.sync()
        return self.focus(window)

    def input_library(self):
        if self.xtst is None:
            self.xtst = C.CDLL("/lib64/libXtst.so.6")
            P, I, U = C.c_void_p, C.c_int, C.c_ulong
            self.bind("XTestQueryExtension", I, [P] + [C.POINTER(I)] * 4, self.xtst)
            self.bind("XTestFakeKeyEvent", I, [P, C.c_uint, I, U], self.xtst)
            self.bind("XTestFakeButtonEvent", I, [P, C.c_uint, I, U], self.xtst)
            self.bind("XTestFakeMotionEvent", I, [P, I, I, I, U], self.xtst)
            values = [I() for _ in range(4)]
            require(self.xtst.XTestQueryExtension(self.display, *(C.byref(v) for v in values)) != 0,
                    "The owned display has no XTest extension")
            self.sync()
        return self.xtst

    def key_codes(self, chord):
        aliases = {"enter": "Return", "escape": "Escape", "esc": "Escape", "tab": "Tab",
                   "space": "space", "backspace": "BackSpace", "delete": "Delete"}
        modifiers = {"ctrl": "Control_L", "control": "Control_L", "shift": "Shift_L",
                     "alt": "Alt_L", "super": "Super_L"}
        if chord.lower() == "shift-tab":
            chord = "shift+Tab"
        parts = chord.split("+")
        require(1 <= len(parts) <= 5 and all(parts), "Invalid key chord")
        names = []
        for part in parts[:-1]:
            require(part.lower() in modifiers, "Unsupported key modifier")
            names.append(modifiers[part.lower()])
        names.append(aliases.get(parts[-1].lower(), parts[-1]))
        codes = []
        for name in names:
            require(re.fullmatch(r"[A-Za-z0-9_]{1,40}", name), "Invalid keysym name")
            symbol = self.lib.XStringToKeysym(name.encode("ascii"))
            code = int(self.lib.XKeysymToKeycode(self.display, symbol)) if symbol else 0
            require(code != 0 and code not in codes, "Unmapped or repeated key")
            codes.append(code)
        return codes

    def key(self, window, chords):
        require(1 <= len(chords) <= 16, "Expected one to sixteen key chords")
        self.selected(window)
        self.focus_check(window)
        library = self.input_library()
        all_codes = [self.key_codes(chord) for chord in chords]
        for codes in all_codes:
            self.selected(window)
            self.focus_check(window)
            keymap = (C.c_char * 32)()
            self.lib.XQueryKeymap(self.display, keymap)
            self.sync()
            pressed_map = bytes(keymap)
            require(not any(pressed_map[code // 8] & (1 << (code % 8)) for code in codes),
                    "A requested key is already held; refusing to release another input")
            pressed = []
            try:
                for code in codes:
                    self.guard(need_ui=True)
                    require(library.XTestFakeKeyEvent(self.display, code, 1, 0) != 0,
                            "Owned key press failed")
                    pressed.append(code)
                self.sync()
            finally:
                # Release only keys this command pressed, and only on the pinned server.
                for code in reversed(pressed):
                    self.guard()
                    require(library.XTestFakeKeyEvent(self.display, code, 0, 0) != 0,
                            "Owned key release failed")
                self.sync()
        self.guard()
        return {"window": hex(window), "chords_sent": len(chords)}

    def pointer(self, window, x, y, button=None):
        metadata = self.selected(window)
        geometry = metadata["geometry"]
        require(0 <= x < geometry["width"] and 0 <= y < geometry["height"],
                "Pointer position must be inside the selected fixture window")
        library = self.input_library()
        require(library.XTestFakeMotionEvent(self.display, -1, geometry["x"] + x,
                geometry["y"] + y, 0) != 0, "Owned pointer motion failed")
        self.sync()
        after_motion = self.selected(window)
        require(after_motion["geometry"] == geometry, "Window geometry changed during pointer motion")
        root, child = C.c_ulong(), C.c_ulong()
        coordinates = [C.c_int() for _ in range(4)]
        mask = C.c_uint()
        require(self.lib.XQueryPointer(self.display, self.root, C.byref(root), C.byref(child),
                *(C.byref(v) for v in coordinates), C.byref(mask)) != 0,
                "Pointer is not on the owned screen")
        self.sync()
        require(self.descendant(child.value, window), "Another window covers the pointer target")
        if button is not None:
            require(button in (1, 2, 3), "Unsupported pointer button")
            require(mask.value & (0x100 << (button - 1)) == 0, "Requested pointer button is already held")
            self.selected(window)
            require(library.XTestFakeButtonEvent(self.display, button, 1, 0) != 0,
                    "Owned button press failed")
            try:
                self.sync()
            finally:
                self.guard()
                require(library.XTestFakeButtonEvent(self.display, button, 0, 0) != 0,
                        "Owned button release failed")
                self.sync()
        self.guard()
        return {"window": hex(window), "relative_point": [x, y], "button": button}

    def capture(self, window, name, whole_root):
        metadata = self.selected(window)
        require(re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9._-]{0,100}\.webp", name),
                "Capture name must be a simple descriptive .webp basename")
        from PIL import ImageGrab, features
        require(features.check("xcb") and features.check("webp"), "Pillow XCB and WebP support are required")
        geometry = metadata["geometry"]
        bbox = None if whole_root else (geometry["x"], geometry["y"],
                geometry["x"] + geometry["width"], geometry["y"] + geometry["height"])
        self.guard(need_ui=True)
        # An explicit xdisplay disables ImageGrab's GNOME desktop fallback.
        image = ImageGrab.grab(bbox=bbox, xdisplay=self.owned.display)
        after_capture = self.selected(window)
        require(after_capture["geometry"] == geometry, "Window geometry changed during capture")
        root_attributes = self.attributes(self.root)
        expected_size = (root_attributes.width, root_attributes.height) if whole_root else (
                        geometry["width"], geometry["height"])
        require(image.size == expected_size, "Captured size differs from the owned fixture viewport")
        directory = self.owned.root / "captures"
        directory.mkdir(mode=0o700, exist_ok=True)
        below_root(str(directory), self.owned.root)
        info = directory.lstat()
        require(stat.S_ISDIR(info.st_mode) and info.st_uid == os.getuid() and info.st_mode & 0o077 == 0,
                "Capture directory must be owned and private")
        output = directory / name
        evidence = {"path": str(output), "state": "not-created-unqualified",
                    "qualified": False, "whole_owned_root": whole_root,
                    "window": hex(window)}
        if not hasattr(self, "capture_evidence"):
            self.capture_evidence = []
        self.capture_evidence.append(evidence)
        original_failure = None
        try:
            fd = os.open(output, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW | os.O_CLOEXEC, 0o600)
            evidence["state"] = "created-unqualified"
            with os.fdopen(fd, "wb") as stream:
                image.save(stream, format="WEBP", lossless=True)
                stream.flush()
                os.fsync(stream.fileno())
            self.guard(need_ui=True)
            result = {"path": str(output), "sha256": hashlib.sha256(output.read_bytes()).hexdigest(),
                      "size": list(expected_size), "whole_owned_root": whole_root,
                      "window": hex(window)}
            evidence.update(result, state="capture-returned-unreviewed")
            return result
        except BaseException as error:
            original_failure = error
            if evidence["state"] != "not-created-unqualified":
                evidence["state"] = "failed-or-partial-unqualified"
            evidence["failure"] = {"type": type(error).__name__, "message": str(error)}
            # Preserve the original private output, even when complete pixels
            # were written before a later source/window/ownership guard failed.
            raise
        finally:
            try:
                image.close()
            except BaseException as error:
                evidence.update(state="failed-or-partial-unqualified",
                                image_close_failure={"type": type(error).__name__, "message": str(error)})
                if original_failure is None:
                    raise


    def accounts_phase(self, window, phase, review, review_sha256, launch, deadline):
        """One reviewed native gesture; returned input is never a UX/test pass."""
        import io
        import math
        import signal
        import time
        from PIL import Image

        plans = source_json(PHASE_PLAN, PHASE_PLAN_SHA256)
        rows = plans["scenes"][launch["scene"]][launch["side"]]["phases"]
        matches = [row for row in rows if row["phase"] == phase]
        require(len(matches) == 1, "Unsupported phase for this fixed scene")
        spec = matches[0]
        directory = self.owned.receipt_path.parent
        packet_path = directory / ("1938-phase-" + phase + ".json")
        # This serial namespace has one operation owner. A failed/incomplete
        # receipt blocks all later steps; no branch/replay/reset is available.
        lock_path = directory / "1938-input.lock"
        import fcntl
        lock = os.open(lock_path, os.O_RDWR | os.O_CREAT | os.O_NOFOLLOW | os.O_CLOEXEC, 0o600)
        try:
            info = os.fstat(lock)
            require(stat.S_ISREG(info.st_mode) and info.st_uid == os.getuid()
                    and info.st_nlink == 1 and stat.S_IMODE(info.st_mode) == 0o600,
                    "Invalid private input lock")
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
            approved, review_bytes = artifact(self.owned, {"path": review, "sha256": review_sha256}, 65536)
            exact_keys(approved, {"version", "phase", "binding", "visible_state", "target", "frames",
                                  "predecessor", "reviewed_unix_ns", "review_reference"}, "1938 root review")
            require(type(approved["version"]) is int and approved["version"] == 1
                    and approved["phase"] == phase and approved["visible_state"] == spec["visible_state"],
                    "Review does not admit this exact scene/phase/state")
            binding = approved["binding"]
            exact_keys(binding, {"side", "scene", "head", "tree", "controller_sha256", "source_binding_sha256",
                                "launch_sha256", "ownership_receipt_sha256", "window", "ui", "server",
                                "ui_sha256", "display", "geometry", "root_size", "viewport", "scale_factor", "theme"}, "frame binding")
            common = {key: launch[key] for key in ("side", "scene", "head", "tree", "controller_sha256",
                       "source_binding_sha256", "ownership_receipt_sha256", "ui", "server", "ui_sha256", "display")}
            require(all(binding[key] == value for key, value in common.items())
                    and binding["launch_sha256"] == launch["_sha256"] and binding["window"] == hex(window)
                    and binding["viewport"] == launch["viewport"]
                    and binding["viewport"] in plans["viewport_sizes"]
                    and type(binding["scale_factor"]) is float and binding["scale_factor"] == 1.0
                    and binding["theme"] == "dark", "Review/source/build/owned-window binding mismatch")
            session = {**common, "window": hex(window), "launch_sha256": launch["_sha256"],
                       "viewport": binding["viewport"], "scale_factor": 1.0, "theme": "dark"}
            for row in rows:
                prior_path = directory / ("1938-phase-" + row["phase"] + ".json")
                exists = prior_path.exists() or prior_path.is_symlink()
                require(exists == (row["index"] < spec["index"]),
                        "Nonlinear/replayed/incomplete phase namespace; no input")
                if exists:
                    prior_bytes, _ = private_bytes(below_root(str(prior_path), self.owned.root), 1048576)
                    prior = json.loads(prior_bytes, object_pairs_hook=no_duplicate_keys)
                    require(type(prior) is dict and prior.get("phase") == row["phase"]
                            and prior.get("session") == session and not prior.get("signals")
                            and prior.get("state") == "operation-returned-awaiting-independent-review",
                            "Prior phase was not exactly completed; no replay/restart")
                    if row["phase"] == spec["predecessor"]:
                        require(approved["predecessor"] == {"path": str(prior_path),
                                "sha256": hashlib.sha256(prior_bytes).hexdigest()}, "Wrong predecessor receipt")
            if spec["predecessor"] is None:
                require(approved["predecessor"] is None, "Initial layout has no predecessor")
            reference = approved["review_reference"]
            require(type(reference) is str and 1 <= len(reference) <= 240, "Explicit root review reference required")
            reviewed = number(approved["reviewed_unix_ns"], "reviewed time", 1)
            require(0 <= time.time_ns() - reviewed <= 300_000_000_000, "Root frame review expired")
            geometry = binding["geometry"]
            exact_keys(geometry, {"x", "y", "width", "height"}, "client geometry")
            require(all(type(geometry[k]) is int for k in geometry) and geometry["x"] >= 0 and geometry["y"] >= 0
                    and 600 <= geometry["width"] <= 1920 and 500 <= geometry["height"] <= 1200,
                    "Unsupported native client geometry")
            expected_size = plans["viewport_sizes"][binding["viewport"]]
            if spec["operation"] != "resize":
                require(geometry["x"] == geometry["y"] == 0
                        and [geometry["width"], geometry["height"]] == expected_size, "Wrong fixed viewport")
            require(type(binding["root_size"]) is list and len(binding["root_size"]) == 2
                    and all(type(n) is int and 0 < n <= 4096 for n in binding["root_size"]), "Invalid root size")
            require(geometry["x"] + geometry["width"] <= binding["root_size"][0]
                    and geometry["y"] + geometry["height"] <= binding["root_size"][1],
                    "Reviewed client is outside the owned root")
            target = approved["target"]
            exact_keys(target, {"role", "label", "subject", "ordinal", "occurrences", "rectangle", "enabled",
                                "unoccluded", "scrollbar"}, "unique reviewed target")
            expected = spec["target"]
            require(all(target[k] == expected[k] for k in ("role", "subject", "ordinal", "occurrences"))
                    and target["enabled"] is True and target["unoccluded"] is True,
                    "Target is not the exact source-derived control/row/ordinal")
            label = spec["labels"][launch["side"]]
            require(target["label"] == label, "Actual side-specific Accounts label differs from fixed phase")

            def rectangle(value):
                require(type(value) is list and len(value) == 4
                        and all(type(n) in (int, float) and math.isfinite(n) for n in value)
                        and value[0] >= 0 and value[1] >= 0 and value[2] > 0 and value[3] > 0
                        and value[0]+value[2] <= geometry["width"]
                        and value[1]+value[3] <= geometry["height"], "Clipped/invalid target rectangle")
                return value

            rect = rectangle(target["rectangle"])
            point = [int(rect[0]+rect[2]/2), int(rect[1]+rect[3]/2)]
            require(rect[0] <= point[0] < rect[0]+rect[2] and rect[1] <= point[1] < rect[1]+rect[3],
                    "No integer target hit point")
            if target["role"].startswith("scrollbar-"):
                exact_keys(target["scrollbar"], {"rail", "thumb"}, "actual scrollbar geometry")
                rail, thumb = rectangle(target["scrollbar"]["rail"]), rectangle(target["scrollbar"]["thumb"])
                require(rail[0] <= rect[0] and rect[0]+rect[2] <= rail[0]+rail[2]
                        and rail[1] <= rect[1] and rect[1]+rect[3] <= rail[1]+rail[3]
                        and rail[0] <= thumb[0] and thumb[0]+thumb[2] <= rail[0]+rail[2]
                        and rail[1] <= thumb[1] and thumb[1]+thumb[3] <= rail[1]+rail[3],
                        "Scroll target/thumb is outside reviewed rail")
                require((target["role"] == "scrollbar-track-below" and rect[1] >= thumb[1]+thumb[3])
                        or (target["role"] == "scrollbar-track-above" and rect[1]+rect[3] <= thumb[1]),
                        "Scroll target must be beyond the actual thumb")
            else:
                require(target["scrollbar"] is None, "Unexpected scrollbar target")
            if spec["operation"] in ("resize", "F8", "Tab", "shift-tab"):
                require(rect == [0, 0, geometry["width"], geometry["height"]], "Nonpointer target is the complete owned window")
            fixture_before = fixture_evidence(self.owned, launch, spec["fixture"]) if spec["operation"] in ("resize", "F8") else None
            exact_keys(approved["frames"], {"root", "client"}, "reviewed root/client frames")

            def pixels(data, size):
                with Image.open(io.BytesIO(data)) as image:
                    require(image.format == "WEBP" and image.size == tuple(size), "Wrong actual reviewed WebP dimensions")
                    return hashlib.sha256(image.convert("RGB").tobytes()).hexdigest()

            reviewed_pixels = {}
            for role, size in [("root", binding["root_size"]), ("client", [geometry["width"], geometry["height"]])]:
                frame = approved["frames"][role]
                exact_keys(frame, {"path", "sha256"}, "reviewed image")
                p = below_root(frame["path"], self.owned.root)
                require(p.parent == self.owned.root/"captures" and p.suffix == ".webp", "Owned native capture required")
                data, info = private_bytes(p, 16777216)
                require(hashlib.sha256(data).hexdigest() == frame["sha256"]
                        and 0 <= reviewed-info.st_mtime_ns <= 300_000_000_000, "Review image changed/expired")
                reviewed_pixels[role] = pixels(data, size)
            packet = {"version": 1, "phase": phase, "session": session, "binding": binding,
                      "state": "started", "signals": [], "review": {"path": review, "sha256": review_sha256},
                      "target": target, "before": {}, "after": {}, "operation": spec["operation"],
                      "fixture_evidence": fixture_before,
                      "pixel_policy": ("fixed-F8-nonpointer-original-held-work" if spec["operation"] == "F8" else
                                       "initial-fixed-nonpointer-layout-original-scene" if spec["operation"] == "resize" else
                                       "exact-reviewed-full-frame-pixels"),
                      "full_frame_pixel_comparison": {},
                      "absolute_deadline_monotonic_ns": int(deadline*1_000_000_000),
                      "limits": "Input/capture observations only. No daemon/provider/UX pass or same-Iced-batch claim."}
            fd = os.open(packet_path, os.O_WRONLY|os.O_CREAT|os.O_EXCL|os.O_NOFOLLOW|os.O_CLOEXEC, 0o600)
            handlers = {}
            with os.fdopen(fd, "w") as stream:
                def save():
                    stream.seek(0); json.dump(packet, stream, indent=2); stream.truncate(); stream.flush(); os.fsync(stream.fileno())

                def stopped(signum, _frame):
                    if len(packet["signals"]) < 64: packet["signals"].append(signum)

                def guard(expected_geometry=geometry):
                    require(not packet["signals"] and time.monotonic() < deadline, "Stopped/expired; no retry or budget reset")
                    check_launch(self.owned, launch)
                    observed = self.selected(window)
                    require(observed["geometry"] == expected_geometry, "Native window geometry changed")
                    if spec["operation"] != "resize" or expected_geometry != geometry:
                        self.focus_check(window)
                    actual_root = self.attributes(self.root)
                    require([actual_root.width, actual_root.height] == binding["root_size"], "Owned root geometry changed")
                    require(private_bytes(below_root(review, self.owned.root),65536)[0] == review_bytes, "Root review changed")

                def frames(label, expected_geometry):
                    guard(expected_geometry)
                    result = {}
                    for role, whole in [("root", True), ("client", False)]:
                        name = "1938-"+phase+"-"+label+"-"+role+"-"+uuid.uuid4().hex[:12]+".webp"
                        result[role] = self.capture(window,name,whole)
                    guard(expected_geometry)
                    return result

                try:
                    for signum in (signal.SIGINT,signal.SIGTERM): handlers[signum]=signal.signal(signum,stopped)
                    save()
                    packet["before"] = frames("fresh",geometry); save()
                    for role,size in [("root",binding["root_size"]),("client",[geometry["width"],geometry["height"]])]:
                        raw,_=private_bytes(below_root(packet["before"][role]["path"],self.owned.root),16777216)
                        actual_pixels = pixels(raw,size)
                        packet["full_frame_pixel_comparison"][role] = {
                            "reviewed_rgb_sha256": reviewed_pixels[role], "current_rgb_sha256": actual_pixels,
                            "equal": actual_pixels == reviewed_pixels[role], "masked": False}
                        if spec["operation"] not in ("resize", "F8"):
                            require(actual_pixels == reviewed_pixels[role], "Current root/client pixels changed; input refused")
                    if spec["operation"] in ("resize", "F8"):
                        # Both are fixed nonpointer operations. Spinner differences
                        # are recorded above; no pointer/hover/Tab equality changes.
                        require(fixture_evidence(self.owned, launch, spec["fixture"]) == fixture_before,
                                "Original scene/work/phase changed before fixed operation; no retry")
                        if spec["operation"] == "F8":
                            self.focus_check(window)  # Window focus is not widget focus.
                    save()
                    guard()
                    packet["dispatch_monotonic_ns"]=time.monotonic_ns();save()
                    if spec["operation"] == "resize":
                        packet["returned"] = self.resize_focus(window,0,0,*expected_size)
                        after_geometry={"x":0,"y":0,"width":expected_size[0],"height":expected_size[1]}
                    elif spec["operation"] == "click":
                        packet["returned"] = self.pointer(window,*point,1);after_geometry=geometry
                    elif spec["operation"] == "motion":
                        packet["returned"] = self.pointer(window,*point);after_geometry=geometry
                    else:
                        require(spec["operation"] in ("Tab","shift-tab","F8"), "No arbitrary key chord")
                        packet["returned"] = self.key(window,[spec["operation"]]);after_geometry=geometry
                    packet["return_monotonic_ns"]=time.monotonic_ns();save()
                    # One bounded rendering opportunity, not a ready-state oracle.
                    require(time.monotonic()+0.2 < deadline, "No original deadline room for capture")
                    time.sleep(0.2)
                    packet["after"]=frames("returned",after_geometry)
                    packet["state"]="operation-returned-awaiting-independent-review";save()
                except BaseException as error:
                    packet.update(state="failed-or-cancelled-no-retry",wrapper_owned_display_teardown_required=True,
                                  error={"type":type(error).__name__,"message":str(error)},
                                  capture_evidence=list(getattr(self,"capture_evidence",[])))
                    try: save()
                    except BaseException: print("Partial1938 receipt could not be completed",file=sys.stderr,flush=True)
                    raise
                finally:
                    for signum,handler in handlers.items(): signal.signal(signum,handler)
            return {"phase":phase,"state":packet["state"],"receipt":str(packet_path),
                    "receipt_sha256":hashlib.sha256(packet_path.read_bytes()).hexdigest(),"UX_result":"UNREVIEWED"}
        finally:
            os.close(lock)


def digest_file(path, maximum):
    """Bounded streaming read; no target executable/import or config evaluation."""
    fd=os.open(path,os.O_RDONLY|os.O_CLOEXEC|os.O_NOFOLLOW)
    try:
        info=os.fstat(fd)
        require(stat.S_ISREG(info.st_mode) and info.st_uid==os.getuid() and info.st_nlink>=1
                and info.st_mode&0o022==0 and 0<info.st_size<=maximum,"Invalid owned source/build file")
        h=hashlib.sha256();total=0
        while True:
            data=os.read(fd,1048576)
            if not data: break
            total+=len(data);require(total<=maximum,"Source/build file exceeds bound");h.update(data)
        after=os.fstat(fd)
        signature=lambda s:(s.st_dev,s.st_ino,s.st_size,s.st_mtime_ns,s.st_ctime_ns,s.st_mode,s.st_uid,s.st_nlink)
        require(total==info.st_size and signature(after)==signature(info),"Source/build file changed during read")
        return h.hexdigest()
    finally: os.close(fd)


def source_json(path, expected):
    fd=os.open(path,os.O_RDONLY|os.O_CLOEXEC|os.O_NOFOLLOW)
    try:
        before=os.fstat(fd)
        require(stat.S_ISREG(before.st_mode) and before.st_uid==os.getuid()
                and before.st_nlink==1 and before.st_mode&0o022==0
                and 0<before.st_size<=1048576,"Invalid frozen controller data file")
        raw=os.read(fd,1048577)
        after=os.fstat(fd)
        signature=lambda s:(s.st_dev,s.st_ino,s.st_size,s.st_mtime_ns,s.st_ctime_ns,s.st_mode,s.st_uid,s.st_nlink)
        require(len(raw)==before.st_size and signature(before)==signature(after)
                and hashlib.sha256(raw).hexdigest()==expected,"Frozen controller data changed")
        return json.loads(raw,object_pairs_hook=no_duplicate_keys)
    finally: os.close(fd)


def verified_digest(launch, path, maximum, expected):
    # The large executable is hashed once per controller invocation. Subsequent
    # guards compare its complete filesystem change identity and the original
    # process/pidfd/executable identity; they never silently accept a new inode.
    info=path.lstat()
    require(stat.S_ISREG(info.st_mode) and not stat.S_ISLNK(info.st_mode)
            and info.st_uid==os.getuid() and info.st_nlink>=1 and info.st_mode&0o022==0,
            "Invalid source/build path")
    signature=(info.st_dev,info.st_ino,info.st_size,info.st_mtime_ns,info.st_ctime_ns,
               info.st_mode,info.st_uid,info.st_nlink)
    cache=launch.setdefault("_digests",{})
    key=str(path)
    if key not in cache:
        require(digest_file(path,maximum)==expected,"Current source/build hash mismatch")
        after=path.lstat()
        require(signature==(after.st_dev,after.st_ino,after.st_size,after.st_mtime_ns,
                after.st_ctime_ns,after.st_mode,after.st_uid,after.st_nlink),"Source/build changed while pinned")
        cache[key]=(signature,expected)
    require(cache[key]==(signature,expected),"Previously verified source/build file changed")


def artifact(owned, reference, maximum):
    exact_keys(reference,{"path","sha256"},"private artifact reference")
    require(type(reference["sha256"]) is str and re.fullmatch(r"[0-9a-f]{64}",reference["sha256"]),"Invalid artifact hash")
    raw,_=private_bytes(below_root(reference["path"],owned.root),maximum)
    require(hashlib.sha256(raw).hexdigest()==reference["sha256"],"Private artifact changed")
    return json.loads(raw,object_pairs_hook=no_duplicate_keys),raw


def fixture_evidence(owned, launch, expected):
    """Read the original private UI log. No transport or watchdog claim."""
    owned.check(need_ui=True)
    ref = launch["fixture_log"]
    raw = read_fixture_log(owned, ref)
    events = fixture_log_events(raw, launch["scene"])
    admits, held, returned, phase = {}, set(), set(), None
    held_event = "matched_quota_observed" if launch["side"] == "after" else "fixture_input_held"
    for event in events:
        if event["event"] == "actual_admission":
            serial = number(event.get("serial"), "original serial")
            require(serial not in admits and type(event.get("kind")) is str
                    and type(event.get("owner")) is int and event["owner"] > 0
                    and type(event.get("id")) is str and event["id"], "Invalid/duplicate original admission")
            if launch["side"] == "before":
                require(event.get("production_ticket_available") is False,
                        "Before task serial must not be presented as production ticket")
            else:
                number(event.get("quota_observation"), "actual collector observation")
            admits[serial] = event
        if event["event"] == held_event:
            serial = number(event.get("serial"), "held serial")
            require(serial in admits and serial not in held, "Held input has no unique original admission")
            held.add(serial)
        if event["event"] == "actual_reducer_completion":
            serial = number(event.get("serial"), "completed serial")
            require(serial in admits and serial not in returned, "Completion has no unique original admission")
            key = "source_oracle_satisfied" if launch["side"] == "after" else "expected_before_consequence_observed"
            require(event.get(key) is True, "Original fixture source oracle failed")
            returned.add(serial)
        if "phase" in event:
            require(type(event["phase"]) is str, "Invalid original fixture phase")
            phase = event["phase"]
    exact_keys(expected, {"phase", "held", "returned"}, "fixed nonpointer phase proof")
    require(phase == expected["phase"], "Original fixture is not at the fixed nonpointer predecessor")
    proof = []
    for state in ("held", "returned"):
        for item in expected[state]:
            exact_keys(item, {"serial", "kind"}, "fixed original work")
            serial = number(item["serial"], "fixed original serial")
            require(serial in admits and admits[serial]["kind"] == item["kind"], "Wrong original work")
            require((serial in returned) if state == "returned" else (serial in held and serial not in returned),
                    "Original work is not in the required held/returned state")
            proof.append({"serial":serial,"kind":item["kind"],"owner":admits[serial]["owner"],
                          "id":admits[serial]["id"],"state":state})
    return {"original_fixture_log":ref,"prefix_sha256":hashlib.sha256(raw).hexdigest(),"bytes":len(raw),
            "scene":launch["scene"],"side":launch["side"],"phase":phase,"original_work":proof,
            "provider_or_watchdog_evidence":False,"widget_focus_inferred":False}


def directory_identity(path, expected):
    require(type(expected) is dict and set(expected)=={"device","inode"},"Invalid directory identity")
    number(expected["device"],"directory device")
    number(expected["inode"],"directory inode",1)
    info=path.lstat()
    require(stat.S_ISDIR(info.st_mode) and not stat.S_ISLNK(info.st_mode) and info.st_uid==os.getuid()
            and info.st_mode&0o022==0 and path.resolve()==path
            and [info.st_dev,info.st_ino]==[expected["device"],expected["inode"]],"Captured directory identity changed")


def check_launch(owned, launch):
    owned.check(need_ui=True)
    require(private_bytes(below_root(launch["_path"],owned.root),1048576)[0]==launch["_bytes"],"Launch binding changed")
    sources=source_json(SOURCE_BINDING,SOURCE_BINDING_SHA256)
    require(launch["source_binding_sha256"]==SOURCE_BINDING_SHA256 and launch["side"] in sources["sides"],"Wrong source side")
    pin=sources["sides"][launch["side"]]
    require(launch["head"]==pin["head"] and launch["tree"]==pin["tree"]
            and launch["source_files"]==pin["files"],"Exact native fixture source pins required")
    require(launch["controller_sha256"]==digest_file(Path(__file__),1048576)
            and launch["ui"]==owned.ui and launch["server"]==owned.server and launch["display"]==owned.display
            and launch["ownership_receipt_sha256"]==owned.receipt_digest,"Controller/owned launch incarnation changed")
    checkout=Path(sources["checkout"])
    require(launch["checkout"]==str(checkout),"Only canonical admitted checkout accepted")
    directory_identity(checkout,launch["checkout_identity"])
    common=Path(sources["common_git_dir"])
    directory_identity(common,launch["common_identity"])
    admin=Path(launch["admin_git_dir"])
    require(admin.is_absolute() and admin.parent==common/"worktrees","Unrecognized worktree admin namespace")
    directory_identity(admin,launch["admin_identity"])
    # Metadata only. No git status/config/filter evaluation or helper invocation.
    metadata=(checkout/".git",admin/"HEAD",admin/"gitdir",admin/"commondir",admin/"index")
    exact_keys(launch["git_metadata_sha256"],{str(p) for p in metadata},"original Git metadata hashes")
    for p in metadata:
        require(digest_file(p,67108864)==launch["git_metadata_sha256"].get(str(p)),"Captured Git metadata changed")
    require((checkout/".git").read_text()=="gitdir: "+str(admin)+"\n"
            and (admin/"HEAD").read_text()==launch["head"]+"\n"
            and (admin/"gitdir").read_text().strip()==str(checkout/".git")
            and (admin/(admin/"commondir").read_text().strip()).resolve()==common,
            "Detached original checkout/common metadata required")
    for name,sha in pin["files"].items():
        path=checkout/name
        current=checkout
        for part in Path(name).parts[:-1]:
            current/=part; info=current.lstat()
            require(stat.S_ISDIR(info.st_mode) and not stat.S_ISLNK(info.st_mode)
                    and info.st_uid==os.getuid() and info.st_mode&0o022==0,"Unsafe source parent")
        verified_digest(launch,path,16777216,sha)
    verified_digest(launch,Path(owned.ui["executable"]),8589934592,launch["ui_sha256"])
    build,_=artifact(owned,launch["build_receipt"],1048576)
    exact_keys(build,{"version","head","tree","checkout","source_files","ui","ui_sha256","matched_builds","original_family_settled"},"matched build receipt")
    require(type(build["version"]) is int and build["version"]==1
            and all(build[k]==launch[k] for k in ("head","tree","checkout","source_files","ui","ui_sha256"))
            and build["original_family_settled"] is True,"Actual matched build/settlement receipt required")
    expected=[['cargo','build','--locked','--message-format=json','--profile','dev-runtime','--no-default-features','--features',features,'--bin',binary]
              for features,binary in [('development','cagents'),('development,host','cagentsd'),('development,ui','cagents-ui')]]
    require(type(build["matched_builds"]) is list and len(build["matched_builds"])==3,"Three separate matched build graphs required")
    for record,argv in zip(build["matched_builds"],expected):
        exact_keys(record,{"argv","original_exit","receipt"},"matched boundary build")
        require(record["argv"]==argv and type(record["original_exit"]) is int and record["original_exit"]==0,"Matched original boundary build failed")
        artifact(owned,record["receipt"],1048576)
    # These are source-pinned observations supplied by the Root-owned adapter,
    # not permission, a replacement for original RAW review, or self-created proof.


def load_launch(owned, path, sha):
    launch,raw=artifact(owned,{"path":path,"sha256":sha},1048576)
    exact_keys(launch,{"version","side","scene","viewport","fixture_log","head","tree","controller_sha256","source_binding_sha256",
                      "ownership_receipt_sha256","ui","server","ui_sha256","display","checkout","checkout_identity",
                      "common_identity","admin_git_dir","admin_identity","git_metadata_sha256","source_files","build_receipt"},"1775 launch binding")
    require(type(launch["version"]) is int and launch["version"]==1
            and launch["scene"] in source_json(PHASE_PLAN,PHASE_PLAN_SHA256)["scenes"]
            and launch["viewport"] in ("wide", "narrow"),"Unsupported inert scene/viewport")
    launch.update(_path=path,_sha256=sha,_bytes=raw)
    check_launch(owned,launch)
    return launch


def window_id(text):
    result = int(text, 0)
    if not 0 < result < 2**32:
        raise argparse.ArgumentTypeError("Expected a nonzero X11 window id")
    return result


def main():
    # NAMES ONLY; refuse conflicting inherited routes instead of clearing them.
    if any(name in FORBIDDEN_NAMES or name.startswith(FORBIDDEN_PREFIXES) for name in os.environ):
        print("REFUSED: conflicting inherited startup/output/fixture/build name",file=sys.stderr)
        return 2
    import time
    deadline=time.monotonic()+20.0
    parser=argparse.ArgumentParser(description="Fixed1938 inert Accounts input/capture only; no execution grant")
    for option in ("expected-root","receipt","display","xauthority","launch","launch-sha256"):
        parser.add_argument("--"+option,required=True)
    parser.add_argument("--window",type=window_id)
    commands=parser.add_subparsers(dest="command",required=True)
    commands.add_parser("list");commands.add_parser("capture")
    phase=commands.add_parser("phase")
    phase.add_argument("--phase",required=True)
    phase.add_argument("--review",required=True);phase.add_argument("--review-sha256",required=True)
    args=parser.parse_args();owned=connection=None
    try:
        owned=Ownership(args)
        launch=load_launch(owned,args.launch,args.launch_sha256)
        require(time.monotonic()<deadline,"Original command deadline expired")
        connection=X11(owned)
        if args.command=="list": result=connection.listing()
        else:
            require(args.window is not None,"Explicit actual owned UI window required")
            connection.selected(args.window)
            if args.command=="capture":
                tag="1938-observe-"+uuid.uuid4().hex[:12]
                result={role:connection.capture(args.window,tag+"-"+role+".webp",whole)
                        for role,whole in [("root",True),("client",False)]}
            else:
                result=connection.accounts_phase(args.window,args.phase,args.review,args.review_sha256,launch,deadline)
        if args.command!="phase":
            check_launch(owned,launch)
            require(time.monotonic()<deadline,"Original command deadline expired; no success conversion")
        print(json.dumps(result,sort_keys=True));return 0
    except BaseException as error:
        print(json.dumps({"state":"REFUSED_NO_RETRY","error":type(error).__name__+": "+str(error),
                          "capture_evidence":list(getattr(connection,"capture_evidence",[])),
                          "wrapper_owned_display_teardown_required":True}),file=sys.stderr,flush=True)
        return 2
    finally:
        if connection: connection.close()
        if owned: owned.close()


if __name__=="__main__":
    raise SystemExit(main())
