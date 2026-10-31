#!/usr/bin/env python3
"""Fixed PR1859 preinit-advisory-pending/narrow owned X11 controller.

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
ALLOWED_ROOT = Path('/tmp/coord16-pr1859-native-fixed-97c1ce31/owned-native')
MAX_WINDOWS = 4096
MAX_DEPTH = 16

FORBIDDEN_NAMES = ('CAGENTS_DEVELOPMENT_ACCOUNT_FIXTURE', 'CAGENTS_DEVELOPMENT_AUTOMATION_FIXTURE', 'WAYLAND_DISPLAY', 'WAYLAND_SOCKET', 'CAGENTS_DEVELOPMENT_NATIVE_IDENTITY_FIXTURE', 'CAGENTS_CHAT_STAGE_OUTPUT', 'CAGENTS_UI_LAUNCH_HANDSHAKE', 'CAGENTS_UI_LAUNCH_RECOVERY_PROGRESS', 'CAGENTS_UI_UPDATE_GUARD_FD', 'CAGENTS_DAEMON_LAUNCH_FENCE_FD', 'CAGENTS_DEVELOPMENT_DAEMON_RECOVERY_FIXTURE', 'CAGENTS_LATENCY_OUTPUT', 'CAGENTS_INSTALL_RESULT_FD', 'CAGENTS_INSTALL_OWNER_CLI', 'CAGENTS_ARCHIVE', 'CAGENTS_CHECKSUMS', 'CAGENTS_REPOSITORY', 'XDG_ACTIVATION_TOKEN', 'DESKTOP_STARTUP_ID', 'GIT_DIR', 'GIT_WORK_TREE', 'GIT_COMMON_DIR', 'GIT_INDEX_FILE', 'GIT_CONFIG', 'GIT_CONFIG_COUNT', 'GIT_CONFIG_PARAMETERS', 'GIT_CONFIG_SYSTEM', 'GIT_CONFIG_GLOBAL', 'GIT_CONFIG_NOSYSTEM', 'GIT_OBJECT_DIRECTORY', 'GIT_ALTERNATE_OBJECT_DIRECTORIES', 'GIT_CEILING_DIRECTORIES', 'GIT_DISCOVERY_ACROSS_FILESYSTEM', 'RUSTC_WRAPPER', 'RUSTC_WORKSPACE_WRAPPER', 'RUSTFLAGS', 'CARGO_ENCODED_RUSTFLAGS')
FORBIDDEN_PREFIXES = ('CARGO_PROFILE_', 'GIT_CONFIG_KEY_', 'GIT_CONFIG_VALUE_')



SOURCE_CONTRACT = Path(__file__).with_name("source-contract.json")
SOURCE_CONTRACT_SHA256 = '619ad94b782ca95dac7c029fbfe270e0df8fc88717159c276080325a0aa3a75a'
PHASE_CONTRACT = Path(__file__).with_name("phase-contract.json")
PHASE_CONTRACT_SHA256 = '0cd33537dde2a0a8793750e9193f39994ca8da2632937d770e7edec1b6f60b8f'
CONTROLLER_CONTRACT = Path(__file__).with_name("controller-contract.json")
CONTROLLER_CONTRACT_SHA256 = '59fbf933257f08c9d384a84c80c988c9cffc938c0706a8a600037190036c0f9c'

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
    def __init__(self, args, require_live_ui=True):
        self.pidfds = []
        self.require_live_ui = require_live_ui
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
        self.check(need_ui=self.require_live_ui and self.ui is not None)
        require(hasattr(os, "pidfd_open"), "A Linux pidfd is required")
        try:
            self.pidfds.append(("server", os.pidfd_open(self.server["pid"], 0)))
            if self.require_live_ui and self.ui is not None:
                self.pidfds.append(("ui", os.pidfd_open(self.ui["pid"], 0)))
            self.check(need_ui=self.require_live_ui and self.ui is not None)
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


class ClientMessageData(C.Union):
    _fields_ = [("b", C.c_char * 20), ("s", C.c_short * 10), ("l", C.c_long * 5)]


class ClientMessageEvent(C.Structure):
    _fields_ = [("type", C.c_int), ("serial", C.c_ulong), ("send_event", C.c_int),
                ("display", C.c_void_p), ("window", C.c_ulong), ("message_type", C.c_ulong),
                ("format", C.c_int), ("data", ClientMessageData)]


class XEvent(C.Union):
    _fields_ = [("xclient", ClientMessageEvent), ("pad", C.c_long * 24)]


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
        self.bind("XSendEvent", I, [P, U, I, C.c_long, C.POINTER(XEvent)])
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


    def bare_escape(self, window):
        """One unmodified Escape; never clear or release an inherited modifier."""
        self.selected(window)
        self.focus_check(window)
        root, child = C.c_ulong(), C.c_ulong()
        coordinates = [C.c_int() for _ in range(4)]
        mask = C.c_uint()
        require(self.lib.XQueryPointer(self.display, window, C.byref(root), C.byref(child),
                *(C.byref(v) for v in coordinates), C.byref(mask)) != 0,
                "Pointer is not on the original screen for bare Escape")
        self.sync()
        require(mask.value & 0xff == 0, "A core keyboard modifier is active; refusing non-bare Escape")
        result = self.key(window, ["Escape"])
        return {**result, "core_modifiers_observed_zero": True, "widget_focus_inferred": False}

    def close_request(self, window):
        """Only WM_DELETE_WINDOW for the exact original owned, focused window."""
        self.selected(window)
        self.focus_check(window)
        protocols = self.property(window, "WM_PROTOCOLS", 4, 32)  # XA_ATOM
        wm = self.lib.XInternAtom(self.display, b"WM_PROTOCOLS", 1)
        delete = self.lib.XInternAtom(self.display, b"WM_DELETE_WINDOW", 1)
        self.sync()
        require(wm and delete and protocols is not None and int(delete) in protocols,
                "Original window does not support WM_DELETE_WINDOW")
        event = XEvent()
        event.xclient.type = 33  # ClientMessage
        event.xclient.serial = 0
        event.xclient.send_event = 1
        event.xclient.display = self.display
        event.xclient.window = window
        event.xclient.message_type = wm
        event.xclient.format = 32
        event.xclient.data.l[0] = delete
        event.xclient.data.l[1] = 0  # CurrentTime; no arbitrary payload/atom API.
        self.selected(window)
        self.focus_check(window)
        require(self.lib.XSendEvent(self.display, window, 0, 0, C.byref(event)) != 0,
                "Owned close request was rejected")
        self.sync()
        self.guard()  # UI may have exited; server/auth/cookie must still be original.
        return {"window": hex(window), "protocol": "WM_DELETE_WINDOW",
                "request_returned": True, "natural_exit_or_cleanup_proven": False}

    def window_state(self, window, launch, predecessor, predecessor_sha256):
        """Read only the original exact window on the still-owned original server."""
        require(not self.owned.require_live_ui, "Post-exit observer is a separate read-only command")
        prior, _ = artifact(self.owned, {"path": predecessor, "sha256": predecessor_sha256}, 1048576)
        contract = source_json(CONTROLLER_CONTRACT, CONTROLLER_CONTRACT_SHA256)
        phases = {row["phase"] for row in contract["steps"][launch["side"]]}
        require(prior.get("phase") in phases and prior.get("session") == session_binding(launch, window)
                and prior.get("state") in set(contract["phase_receipt_states_usable_only_as_input_predecessors"])
                    | {contract["terminal_close_request_state"]}
                and not prior.get("signals") and prior.get("before", {}).get("root")
                and prior.get("before", {}).get("client"), "Earlier original live-UI phase proof required")
        check_launch(self.owned, launch, need_ui=False)
        pending, seen = [(self.root, 0)], set()
        while pending:
            candidate, depth = pending.pop()
            require(candidate not in seen and len(seen) < MAX_WINDOWS and depth <= MAX_DEPTH,
                    "Owned observation tree exceeded its bounds")
            seen.add(candidate)
            _, children = self.tree(candidate)
            pending.extend((child, depth + 1) for child in children)
        present = window in seen
        metadata = self.metadata(window) if present else None
        if present:
            require(metadata["pid"] == self.owned.ui["pid"] and metadata["expected_class"],
                    "Original window identity changed; no replacement/reselection")
        check_launch(self.owned, launch, need_ui=False)
        return {"window": hex(window), "present": present, "metadata": metadata,
                "original_ui": self.owned.ui, "original_server": self.owned.server,
                "predecessor": {"path": predecessor, "sha256": predecessor_sha256},
                "ui_live_cookie_required": False, "natural_exit_or_cleanup_proven": False,
                "limits": "Original executor must separately prove UI pidfd/wait exit BEFORE supervised cleanup."}

    def preinit_phase(self, window, phase, review, review_sha256, launch, deadline):
        """Exactly one fixed reviewed substep; no automatic native UX/test verdict."""
        import fcntl
        import io
        import math
        import signal
        import time
        from PIL import Image, ImageChops

        contract = source_json(CONTROLLER_CONTRACT, CONTROLLER_CONTRACT_SHA256)
        rows = {row["phase"]: row for row in contract["steps"][launch["side"]]}
        require(phase in rows, "No arbitrary phase/action/key API")
        spec = rows[phase]
        directory = self.owned.receipt_path.parent
        packet_path = directory / ("1859-phase-" + phase + ".json")
        lock_path = directory / "1859-input.lock"
        lock = os.open(lock_path, os.O_RDWR | os.O_CREAT | os.O_NOFOLLOW | os.O_CLOEXEC, 0o600)
        try:
            info = os.fstat(lock)
            require(stat.S_ISREG(info.st_mode) and info.st_uid == os.getuid() and info.st_nlink == 1
                    and stat.S_IMODE(info.st_mode) == 0o600, "Invalid private input lock")
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
            approved, review_bytes = artifact(self.owned, {"path": review, "sha256": review_sha256}, 65536)
            exact_keys(approved, set(contract["review_exact_keys"]), "1859 root frame review")
            require(type(approved["version"]) is int and approved["version"] == 1
                    and approved["phase"] == phase and approved["visible_state"] == spec["visible_state"],
                    "Review does not admit this exact fixed substep")
            binding = approved["binding"]
            exact_keys(binding, set(contract["review_binding_exact_keys"]), "frame binding")
            session = session_binding(launch, window)
            require(all(binding[key] == value for key, value in session.items()),
                    "Review source/build/launch/window/session binding mismatch")
            require(type(binding["scale_factor"]) is float and binding["scale_factor"] == 1.0,
                    "Exact native scale1.0 required; window focus never implies widget focus")
            existing = {}
            for name in rows:
                path = directory / ("1859-phase-" + name + ".json")
                if path.exists() or path.is_symlink():
                    b, _ = private_bytes(below_root(str(path), self.owned.root), 1048576)
                    prior = json.loads(b, object_pairs_hook=no_duplicate_keys)
                    require(prior.get("phase") == name and prior.get("session") == session
                            and not prior.get("signals") and prior.get("state") in
                            contract["phase_receipt_states_usable_only_as_input_predecessors"],
                            "Failed/incomplete/original phase namespace cannot be replayed or bypassed")
                    existing[name] = (prior, {"path": str(path), "sha256": hashlib.sha256(b).hexdigest()})
            require(phase not in existing, "Phase already exists; no retry/reset/relaunch")
            ancestor, link = set(), approved["predecessor"]
            allowed = spec["predecessors"]
            while link is not None:
                match = [name for name, (_prior, r) in existing.items() if r == link]
                require(len(match) == 1 and match[0] in allowed and match[0] not in ancestor,
                        "Exact original immediate predecessor/branch required")
                name = match[0]
                ancestor.add(name)
                prior = existing[name][0]
                link, allowed = prior["predecessor"], rows[name]["predecessors"]
            require(not allowed and set(existing) == ancestor,
                    "Nonlinear/skipped/incomplete fixed substep namespace")
            reference = approved["review_reference"]
            require(type(reference) is str and 1 <= len(reference) <= 240, "Explicit independent review reference required")
            reviewed = number(approved["reviewed_unix_ns"], "actual review time", 1)
            require(0 <= time.time_ns() - reviewed <= 300_000_000_000, "Root frame review expired")
            visual = approved["visual"]
            exact_keys(visual, set(contract["review_visual_exact_keys"]), "actual native visible state")
            require(visual["modal_title"] == spec["modal_title"]
                    and type(visual["prompt_visible_exact"]) is bool
                    and (not spec["prompt_visible_exact"] or visual["prompt_visible_exact"] is True)
                    and visual["notice_visibility"] in ("not-assessed", "control-present", "control-absent", "panel-open", "panel-closed")
                    and (spec["notice_visibility"] == "not-assessed"
                         or visual["notice_visibility"] == spec["notice_visibility"]),
                    "Missing actual reviewed prompt/modal/notice state; no fabricated branch")
            geometry = binding["geometry"]
            exact_keys(geometry, {"x", "y", "width", "height"}, "client geometry")
            require(all(type(geometry[k]) is int for k in geometry) and geometry["x"] >= 0
                    and geometry["y"] >= 0 and 600 <= geometry["width"] <= 1920
                    and 500 <= geometry["height"] <= 1200, "Unsupported native geometry")
            if spec["operation"] != "resize":
                require(geometry == {"x": 0, "y": 0, "width": 900, "height": 900}, "Exact900x900 narrow client required")
            root_size = binding["root_size"]
            require(type(root_size) is list and len(root_size) == 2
                    and all(type(n) is int and 0 < n <= 4096 for n in root_size)
                    and geometry["x"] + geometry["width"] <= root_size[0]
                    and geometry["y"] + geometry["height"] <= root_size[1], "Client outside original root")
            target = approved["target"]
            exact_keys(target, set(contract["target_exact_keys"]), "exact unique native target")
            require(all(target[k] == spec["target"][k] for k in spec["target"])
                    and target["enabled"] is True and target["unoccluded"] is True
                    and target["scrollbar"] is None, "Wrong source control or nonpointer target")
            rect = target["rectangle"]
            require(type(rect) is list and len(rect) == 4 and all(type(v) in (int, float)
                    and math.isfinite(v) for v in rect) and rect[0] >= 0 and rect[1] >= 0
                    and rect[2] > 0 and rect[3] > 0 and rect[0] + rect[2] <= geometry["width"]
                    and rect[1] + rect[3] <= geometry["height"], "Clipped/invalid actual target rectangle")
            if spec["operation"] not in ("click", "motion"):
                require(rect == [0, 0, geometry["width"], geometry["height"]], "Nonpointer target is the full original window")
            point = [int(rect[0] + rect[2] / 2), int(rect[1] + rect[3] / 2)]
            require(rect[0] <= point[0] < rect[0] + rect[2] and rect[1] <= point[1] < rect[1] + rect[3], "No integer hit point")
            exact_keys(approved["frames"], {"root", "client"}, "actual reviewed root/client captures")
            reviewed_rgb = {}

            def rgb(raw, size):
                with Image.open(io.BytesIO(raw)) as image:
                    require(image.format == "WEBP" and image.size == tuple(size), "Wrong reviewed lossless native frame geometry")
                    converted = image.convert("RGB")
                    try:
                        return converted.tobytes()
                    finally:
                        converted.close()

            for role, size in [("root", root_size), ("client", [geometry["width"], geometry["height"]])]:
                frame = approved["frames"][role]
                exact_keys(frame, {"path", "sha256"}, "original reviewed native frame")
                p = below_root(frame["path"], self.owned.root)
                require(p.parent == self.owned.root / "captures" and p.suffix == ".webp", "Only original owned native captures")
                b, st = private_bytes(p, 16777216)
                require(hashlib.sha256(b).hexdigest() == frame["sha256"]
                        and 0 <= reviewed - st.st_mtime_ns <= 300_000_000_000, "Review frame changed/expired")
                reviewed_rgb[role] = rgb(b, size)
            attempt_identity = None
            if spec["attempt"] is not None:
                require(spec["attempt"] in existing, "Original native Send anchor missing")
                anchor = existing[spec["attempt"]][0]
                original_state = anchor["fixture_after"]["latest"]["record"]["state"]
                require(original_state is not None, "Original Send has no actual state; no guessed attempt")
                attempt_identity = {"conversation": original_state["conversation"]}
                if spec["operation"] == "await-error":
                    dispatched = number(anchor["dispatch_monotonic_ns"], "original Send clock", 1) / 1_000_000_000
                    require(dispatched <= time.monotonic(), "Original Send clock is not current")
                    deadline = min(deadline, dispatched + 75.0)
            packet = {"version": 1, "phase": phase, "source_phase": spec["plan_phase"],
                      "session": session, "binding": binding, "predecessor": approved["predecessor"],
                      "state": "started", "signals": [], "review": {"path": review, "sha256": review_sha256},
                      "target": target, "visual": visual, "before": {}, "after": {},
                      "operation": spec["operation"], "fixture_before": None, "fixture_after": None,
                      "coverage_missing": spec["coverage_missing"], "full_frame_pixel_comparison": {},
                      "absolute_deadline_monotonic_ns": int(deadline * 1_000_000_000),
                      "pixel_policy": "exact-reviewed-full-frame-pixels" if spec["operation"] in
                          ("click", "motion", "Tab", "shift-tab") else "fixed-nonpointer-full-unmasked-differences",
                      "limits": "Native input/observation only; no provider cleanup, natural exit, widget focus or UX pass."}
            fd = os.open(packet_path, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW | os.O_CLOEXEC, 0o600)
            handlers = {}
            with os.fdopen(fd, "w") as stream:
                def save():
                    stream.seek(0)
                    json.dump(packet, stream, indent=2)
                    stream.truncate()
                    stream.flush()
                    os.fsync(stream.fileno())

                def stopped(signum, _frame):
                    if len(packet["signals"]) < 64:
                        packet["signals"].append(signum)

                def guard(expected_geometry=geometry):
                    require(not packet["signals"] and time.monotonic() < deadline,
                            "Original phase stopped/expired; no retry or budget reset")
                    check_launch(self.owned, launch)
                    require(self.selected(window)["geometry"] == expected_geometry, "Original window geometry changed")
                    if spec["operation"] != "resize" or expected_geometry != geometry:
                        self.focus_check(window)
                    root = self.attributes(self.root)
                    require([root.width, root.height] == root_size, "Owned root geometry changed")
                    require(private_bytes(below_root(review, self.owned.root), 65536)[0] == review_bytes, "Root review changed")

                def frames(label, expected_geometry):
                    guard(expected_geometry)
                    result = {}
                    for role, whole in [("root", True), ("client", False)]:
                        name = "1859-" + phase + "-" + label + "-" + role + "-" + uuid.uuid4().hex[:12] + ".webp"
                        result[role] = self.capture(window, name, whole)
                    guard(expected_geometry)
                    return result

                def difference(role, current_rgb, size):
                    a = reviewed_rgb[role]
                    equal = a == current_rgb
                    out = {"reviewed_rgb_sha256": hashlib.sha256(a).hexdigest(),
                           "current_rgb_sha256": hashlib.sha256(current_rgb).hexdigest(),
                           "equal": equal, "masked": False,
                           "different_channels": sum(x != y for x, y in zip(a, current_rgb))}
                    if not equal:
                        before = Image.frombytes("RGB", tuple(size), a)
                        current = Image.frombytes("RGB", tuple(size), current_rgb)
                        delta = ImageChops.difference(before, current)
                        try:
                            out["difference_bbox"] = delta.getbbox()
                            path = self.owned.root / "captures" / ("1859-" + phase + "-all-RGB-differences-" + role + "-" + uuid.uuid4().hex[:12] + ".webp")
                            evidence = {"path": str(path), "state": "not-created-unqualified", "qualified": False,
                                        "kind": "all-unmasked-RGB-differences"}
                            self.capture_evidence.append(evidence)
                            dfd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW | os.O_CLOEXEC, 0o600)
                            evidence["state"] = "created-unqualified"
                            with os.fdopen(dfd, "wb") as output:
                                delta.save(output, format="WEBP", lossless=True)
                                output.flush()
                                os.fsync(output.fileno())
                            out["all_unmasked_RGB_differences"] = {"path": str(path), "sha256": hashlib.sha256(path.read_bytes()).hexdigest()}
                            evidence.update(state="difference-returned-unreviewed", **out["all_unmasked_RGB_differences"])
                        finally:
                            delta.close()
                            current.close()
                            before.close()
                    return out

                try:
                    for signum in (signal.SIGINT, signal.SIGTERM):
                        handlers[signum] = signal.signal(signum, stopped)
                    save()
                    fixture_before = fixture_evidence(self.owned, launch)
                    packet["fixture_before"] = fixture_before
                    require(fixture_before == approved["fixture_log"], "Original reviewed ui.log prefix changed; no guessed phase")
                    condition = "card-present" if spec["operation"] == "await-error" else spec["condition"]
                    require(phase_condition(fixture_before, condition, launch["side"], attempt_identity),
                            "MISSING_ACTUAL_STATE: " + spec["condition"] + "; no positive skip, synthetic callback or retry")
                    packet["before"] = frames("fresh", geometry)
                    save()
                    for role, size in [("root", root_size), ("client", [geometry["width"], geometry["height"]])]:
                        b, _ = private_bytes(below_root(packet["before"][role]["path"], self.owned.root), 16777216)
                        compare = difference(role, rgb(b, size), size)
                        packet["full_frame_pixel_comparison"][role] = compare
                        if spec["operation"] in ("click", "motion", "Tab", "shift-tab"):
                            require(compare["equal"], "Current whole root/client pixels changed; input refused")
                    save()
                    guard()
                    # Nonpointer exceptions cannot reuse an obsolete actual log/attempt/modal review.
                    if spec["operation"] not in ("observe", "await-error"):
                        require(fixture_evidence(self.owned, launch) == fixture_before,
                                "Original sparse state/log changed before native gesture; no retry")
                    packet["dispatch_monotonic_ns"] = time.monotonic_ns()
                    save()
                    after_geometry = geometry
                    op = spec["operation"]
                    if op == "resize":
                        packet["returned"] = self.resize_focus(window, 0, 0, 900, 900)
                        after_geometry = {"x": 0, "y": 0, "width": 900, "height": 900}
                    elif op == "click":
                        packet["returned"] = self.pointer(window, *point, 1)
                    elif op == "motion":
                        packet["returned"] = self.pointer(window, *point)
                    elif op in ("Tab", "shift-tab"):
                        packet["returned"] = self.key(window, [op])
                    elif op == "Escape":
                        require(spec["modal_title"] == "Keep unsaved work before closing"
                                and visual["modal_title"] == spec["modal_title"], "Bare Escape only in actual reviewed close modal")
                        self.focus_check(window)
                        packet["returned"] = self.bare_escape(window)
                    elif op == "WM_DELETE_WINDOW":
                        self.focus_check(window)
                        packet["returned"] = self.close_request(window)
                    else:
                        require(op in ("observe", "await-error"), "No arbitrary operation")
                        packet["returned"] = {"input_sent": False}
                        if op == "await-error":
                            while True:
                                guard()
                                observed = fixture_evidence(self.owned, launch)
                                packet["fixture_after"] = observed
                                if phase_condition(observed, "expected-error", launch["side"], attempt_identity):
                                    break
                                require(time.monotonic() + 0.2 < deadline,
                                        "MISSING_EXPECTED_ERROR_BEFORE_ORIGINAL_SEND_PLUS75S; no watchdog pass")
                                time.sleep(0.2)  # Bounded passive log observation, never an equality/input retry.
                    packet["return_monotonic_ns"] = time.monotonic_ns()
                    save()
                    if phase == "natural-close":
                        # Invocation only. Do not demand a live UI after asking it
                        # to close and do not convert any guard failure to success.
                        require(not packet["signals"] and time.monotonic() < deadline,
                                "Final close request interrupted/expired; no exit conversion")
                        check_launch(self.owned, launch, need_ui=False)
                        self.guard()
                        packet["state"] = "native-close-request-returned-exit-unqualified"
                        packet["post_request_observation_missing"] = {
                            "reason": "No live-UI capture attempted after close request; original executor owns pre-cleanup UI wait/pidfd plus exact window-state."}
                        save()
                    else:
                        require(time.monotonic() + 0.2 < deadline, "No original deadline room for returned capture")
                        time.sleep(0.2)
                        packet["after"] = frames("returned", after_geometry)
                        packet["fixture_after"] = fixture_evidence(self.owned, launch)
                        if phase in ("send-1", "send-2"):
                            before_state = fixture_before["latest"]["record"]["state"]
                            after_state = packet["fixture_after"]["latest"]["record"]["state"]
                            require(before_state is not None and after_state is not None
                                    and before_state["conversation"] != after_state["conversation"],
                                    "Native Send has no observed new source-owned conversation; no guessed dispatch/retry")
                            packet["actual_send_conversation"] = after_state["conversation"]
                            packet["observed_target_generation"] = after_state["target_generation"]
                        packet["state"] = ("observation-returned-awaiting-independent-review" if op in
                                           ("observe", "await-error") else "operation-returned-awaiting-independent-review")
                        save()
                except BaseException as error:
                    packet.update(state="failed-or-missing-no-retry", wrapper_owned_display_teardown_required=True,
                                  error={"type": type(error).__name__, "message": str(error)},
                                  capture_evidence=list(getattr(self, "capture_evidence", [])))
                    try:
                        save()
                    except BaseException:
                        print("Partial1859 receipt could not be completed", file=sys.stderr, flush=True)
                    raise
                finally:
                    for signum, handler in handlers.items():
                        signal.signal(signum, handler)
            return {"phase": phase, "state": packet["state"], "receipt": str(packet_path),
                    "receipt_sha256": hashlib.sha256(packet_path.read_bytes()).hexdigest(),
                    "coverage_missing": packet["coverage_missing"], "UX_result": "UNREVIEWED"}
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


def session_binding(launch, window):
    keys = ("side", "scene", "head", "tree", "controller_sha256", "controller_contract_sha256",
            "source_contract_sha256", "phase_contract_sha256", "ownership_receipt_sha256",
            "ui", "server", "ui_sha256", "display")
    return {**{key: launch[key] for key in keys}, "window": hex(window),
            "launch_sha256": launch["_sha256"], "viewport": "narrow",
            "scale_factor": 1.0, "theme": "dark"}


def fixture_evidence(owned, launch):
    """Bounded original ui.log STDERR evidence; sparse records are not lifecycle proof."""
    owned.check(need_ui=True)
    ref = launch["fixture_log"]
    exact_keys(ref, {"path", "device", "inode", "uid", "mode"}, "original private ui.log")
    path = below_root(ref["path"], owned.root)
    raw, info = private_bytes(path, 4194304)
    require([info.st_dev, info.st_ino, info.st_uid, stat.S_IMODE(info.st_mode)] ==
            [ref["device"], ref["inode"], ref["uid"], ref["mode"]]
            and ref["uid"] == os.getuid() and ref["mode"] == 0o600,
            "Original ui.log incarnation/permissions changed")
    require(raw.endswith(b"\n"), "Partial original ui.log; no input or equal-frame hunt")
    records, offset = [], 0
    first = None
    boundary = "offline fixed process; actual reducer/runtime only; no provider/auth/tools/history qualification"
    program = str(Path(launch["checkout"]) / "tests/fixtures/claude-preinit-advisory-pending.py")
    for line in raw.splitlines(keepends=True):
        start = offset
        offset += len(line)
        if not line.startswith(b"PREINIT_NATIVE "):
            continue
        require(len(line) <= 16384, "Original PREINIT_NATIVE record exceeds bound")
        event = json.loads(line[len(b"PREINIT_NATIVE "):], object_pairs_hook=no_duplicate_keys)
        exact_keys(event, {"event", "scene", "card", "state", "qualified", "boundary"},
                   "actual PREINIT_NATIVE record")
        require(event["event"] in ("installed", "update", "foreground-refused")
                and event["scene"] == "preinit-advisory-pending" and event["qualified"] is False
                and event["boundary"] == boundary, "Unexpected original fixture record")
        require(type(event["card"]) is str and uuid.UUID(event["card"]).int != 0,
                "Invalid original fixture card")
        state = event["state"]
        if state is not None:
            exact_keys(state, {"conversation", "target_generation", "ready", "busy", "exited",
                               "error", "item_count", "original_program"}, "actual sparse runtime state")
            require(type(state["conversation"]) is str and uuid.UUID(state["conversation"]).int != 0,
                    "Invalid original conversation")
            require(all(type(state[key]) is bool for key in ("ready", "busy", "exited"))
                    and (state["error"] is None or type(state["error"]) is str and len(state["error"]) <= 512)
                    and state["original_program"] == program, "Unexpected actual runtime state")
            require(number(state["target_generation"], "original target generation") < 2**64
                    and number(state["item_count"], "actual item count") < 2**64,
                    "Original runtime number exceeds source type")
        if first is None:
            require(event["event"] == "installed" and state is not None,
                    "Missing original installed record; no log adoption")
            first = event
        else:
            require(event["event"] != "installed" and event["card"] == first["card"],
                    "Original fixture card replaced or installed twice")
        records.append({"offset_start": start, "offset_end": offset,
                        "record_sha256": hashlib.sha256(line).hexdigest(), "record": event})
        require(len(records) <= 4096, "Original PREINIT_NATIVE record count exceeds bound")
    require(records, "Original ui.log has no PREINIT_NATIVE evidence")
    return {"original_ui_log": ref, "offset_start": 0, "offset_end": len(raw),
            "prefix_sha256": hashlib.sha256(raw).hexdigest(), "record_count": len(records),
            "latest": records[-1], "original_card": first["card"],
            "installed_conversation_history_only": first["state"]["conversation"],
            "provider_pid_pending_phase_or_cleanup_proven": False}


def phase_condition(evidence, condition, side, attempt=None):
    """Only actual sparse fields; modal/prompt visibility remains independent native review."""
    state = evidence["latest"]["record"]["state"]
    if condition == "any":
        return True
    if state is None:
        return False  # Card absence is not cleanup or an error receipt.
    if attempt is not None and state["conversation"] != attempt["conversation"]:
        return False
    if condition == "card-present":
        return True
    if condition == "not-ready":
        return state["ready"] is False
    if condition == "pending":
        return state["ready"] is False and state["exited"] is False and state["error"] is None
    require(condition == "expected-error", "No arbitrary state predicate")
    expected = ("Unsupported Claude stream-json frame type future_advisory" if side == "before"
                else "Provider initialization timed out before handshake completion")
    return state["ready"] is False and type(state["error"]) is str and expected in state["error"]


def directory_identity(path, expected):
    require(type(expected) is dict and set(expected)=={"device","inode"},"Invalid directory identity")
    number(expected["device"],"directory device")
    number(expected["inode"],"directory inode",1)
    info=path.lstat()
    require(stat.S_ISDIR(info.st_mode) and not stat.S_ISLNK(info.st_mode) and info.st_uid==os.getuid()
            and info.st_mode&0o022==0 and path.resolve()==path
            and [info.st_dev,info.st_ino]==[expected["device"],expected["inode"]],"Captured directory identity changed")


def check_launch(owned, launch, need_ui=True):
    owned.check(need_ui=need_ui)
    require(private_bytes(below_root(launch["_path"],owned.root),1048576)[0]==launch["_bytes"],"Launch binding changed")
    sources=source_json(SOURCE_CONTRACT,SOURCE_CONTRACT_SHA256)
    controller=source_json(CONTROLLER_CONTRACT,CONTROLLER_CONTRACT_SHA256)
    source_json(PHASE_CONTRACT,PHASE_CONTRACT_SHA256)
    require(launch["source_contract_sha256"]==SOURCE_CONTRACT_SHA256
            and launch["controller_contract_sha256"]==CONTROLLER_CONTRACT_SHA256
            and launch["phase_contract_sha256"]==PHASE_CONTRACT_SHA256
            and launch["side"] in sources["paired_sources"],"Wrong exact source/phase/controller contract")
    pin=sources["paired_sources"][launch["side"]]
    files={name:value["sha256"] for name,value in pin["files"].items()}
    require(launch["head"]==pin["head"] and launch["tree"]==pin["tree"]
            and launch["source_files"]==files,"Exact native fixture source pins required")
    require(launch["controller_sha256"]==digest_file(Path(__file__),1048576)
            and launch["ui"]==owned.ui and launch["server"]==owned.server and launch["display"]==owned.display
            and launch["ownership_receipt_sha256"]==owned.receipt_digest,"Controller/owned launch incarnation changed")
    checkout=Path(controller["canonical_checkout"])
    require(launch["checkout"]==str(checkout),"Only canonical admitted checkout accepted")
    directory_identity(checkout,launch["checkout_identity"])
    common=Path(controller["canonical_common_git_dir"])
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
    for name,sha in files.items():
        path=checkout/name
        current=checkout
        for part in Path(name).parts[:-1]:
            current/=part; info=current.lstat()
            require(stat.S_ISDIR(info.st_mode) and not stat.S_ISLNK(info.st_mode)
                    and info.st_uid==os.getuid() and info.st_mode&0o022==0,"Unsafe source parent")
        verified_digest(launch,path,16777216,sha)
    for name in sources["optional_tracked_config_absence"][launch["side"]]:
        path=checkout/name
        require(not path.exists() and not path.is_symlink(),"Source-pinned optional configuration absence changed")
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


def load_launch(owned, path, sha, need_ui=True):
    launch, raw = artifact(owned, {"path": path, "sha256": sha}, 1048576)
    contract = source_json(CONTROLLER_CONTRACT, CONTROLLER_CONTRACT_SHA256)
    exact_keys(launch, set(contract["launch_exact_keys"]), "1859 original launch binding")
    require(type(launch["version"]) is int and launch["version"] == 1
            and launch["scene"] == "preinit-advisory-pending" and launch["viewport"] == "narrow"
            and launch["side"] in ("before", "after"), "Only the fixed1859 narrow pair is accepted")
    exact_keys(launch["fixture_log"], {"path", "device", "inode", "uid", "mode"}, "original private ui.log")
    require(launch["fixture_log"]["uid"] == os.getuid() and launch["fixture_log"]["mode"] == 0o600,
            "Original private ui.log must be owned0600")
    launch.update(_path=path, _sha256=sha, _bytes=raw)
    check_launch(owned, launch, need_ui=need_ui)
    return launch


def window_id(text):
    result = int(text, 0)
    if not 0 < result < 2**32:
        raise argparse.ArgumentTypeError("Expected a nonzero X11 window id")
    return result


def main():
    require(sys.flags.isolated == 1 and sys.dont_write_bytecode and sys.flags.optimize == 0,
            "Invoke the original interpreter with -I -B and without optimization")
    # NAMES ONLY. No authority values, environment repair, ambient display or generic action API.
    if any(name in FORBIDDEN_NAMES or name.startswith(FORBIDDEN_PREFIXES) for name in os.environ):
        print("REFUSED: conflicting inherited startup/output/fixture/build name", file=sys.stderr)
        return 2
    import time
    started = time.monotonic()
    parser = argparse.ArgumentParser(description="Fixed1859 preinit native observations only; no execution grant")
    for option in ("expected-root", "receipt", "display", "xauthority", "launch", "launch-sha256"):
        parser.add_argument("--" + option, required=True)
    parser.add_argument("--window", type=window_id)
    commands = parser.add_subparsers(dest="command", required=True)
    commands.add_parser("list")
    commands.add_parser("capture")
    phase = commands.add_parser("phase")
    phase.add_argument("--phase", required=True)
    phase.add_argument("--review", required=True)
    phase.add_argument("--review-sha256", required=True)
    observation = commands.add_parser("window-state")
    observation.add_argument("--predecessor", required=True)
    observation.add_argument("--predecessor-sha256", required=True)
    args = parser.parse_args()
    owned = connection = None
    passive_exit = args.command == "window-state"
    deadline = started + (75.0 if args.command == "phase" and args.phase in
                          ("first-consequence", "deadline-consequence") else 20.0)
    try:
        # This is the only no-live-UI constructor path. It cannot send input/capture.
        owned = Ownership(args, require_live_ui=not passive_exit)
        launch = load_launch(owned, args.launch, args.launch_sha256, need_ui=not passive_exit)
        require(time.monotonic() < deadline, "Original command deadline expired")
        connection = X11(owned)
        if args.command == "list":
            result = connection.listing()
        else:
            require(args.window is not None, "Explicit original owned UI window required")
            if passive_exit:
                result = connection.window_state(args.window, launch, args.predecessor, args.predecessor_sha256)
            else:
                connection.selected(args.window)
                if args.command == "capture":
                    tag = "1859-observe-" + uuid.uuid4().hex[:12]
                    result = {role: connection.capture(args.window, tag + "-" + role + ".webp", whole)
                              for role, whole in [("root", True), ("client", False)]}
                    result["fixture_log"] = fixture_evidence(owned, launch)
                    result["session"] = session_binding(launch, args.window)
                else:
                    result = connection.preinit_phase(args.window, args.phase, args.review,
                                                      args.review_sha256, launch, deadline)
        if args.command != "phase":
            check_launch(owned, launch, need_ui=not passive_exit)
            require(time.monotonic() < deadline, "Original command deadline expired; no success conversion")
        print(json.dumps(result, sort_keys=True))
        return 0
    except BaseException as error:
        print(json.dumps({"state": "REFUSED_NO_RETRY", "error": type(error).__name__ + ": " + str(error),
                          "capture_evidence": list(getattr(connection, "capture_evidence", [])),
                          "wrapper_owned_display_teardown_required": True}), file=sys.stderr, flush=True)
        return 2
    finally:
        if connection:
            connection.close()
        if owned:
            owned.close()


if __name__=="__main__":
    raise SystemExit(main())
