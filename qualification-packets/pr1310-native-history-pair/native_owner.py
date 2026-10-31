"""Finite original owner for PR1310; standalone admission is deliberately absent.

The future reviewed entry supplies actual outer authorization, one original
Custody object, source/tool/matched-build pins and absolute budgets. These pins
are expectations to check against real resources, never authority receipts.
No native module is imported by this file. Nothing launches on import.
"""

from dataclasses import dataclass
from functools import wraps
import hashlib
import inspect
import json
import math
import os
from pathlib import Path
import re
import select
import signal
import stat
import subprocess
import threading
import time
import uuid


CUSTODY_SHA = "d9eeec3f2e228e4844695de096f85ecf465381ebd0a2c7a85e964b7103ebe0d9"
BUILDER_SHA = "77b0694f9b3e2f7cec55ba4bbd6e2a534e5f661c5340cef342ad2669f11d5b01"
ARMS = {
    "before": ("a174dc7e7aa651745c95714971054fc4c270c5a3", "2d557d0cd3757a01ee9ca2da32d15f211ce464b6"),
    "after": ("9dc2a70caebbfc972e537712b9f9326843f232d0", "fcb1b582d894dc46590cc8fc8f20e9cb17920a0a"),
}
PRIVATE_KEYS = ("XDG_CONFIG_HOME", "XDG_DATA_HOME", "XDG_STATE_HOME",
                "XDG_CACHE_HOME", "XDG_RUNTIME_DIR", "TMPDIR")
ENDPOINT_KEYS = ("DISPLAY", "XAUTHORITY", "DBUS_SESSION_BUS_ADDRESS", "AT_SPI_BUS_ADDRESS")
UNSUPPORTED_ENDPOINT_KEYS = ("WAYLAND_DISPLAY", "WAYLAND_SOCKET", "DBUS_STARTER_ADDRESS",
                             "DBUS_STARTER_BUS_TYPE", "SESSION_MANAGER", "LISTEN_PID", "LISTEN_FDS")
MAX_OUTPUT = 262144


class Hold(RuntimeError):
    pass


def require(value, reason):
    if not value:
        raise Hold(reason)


def digest(data):
    return hashlib.sha256(data).hexdigest()


def identity(info):
    return (info.st_dev, info.st_ino, info.st_uid, stat.S_IMODE(info.st_mode), info.st_nlink)


def unchanged(info):
    return (*identity(info), info.st_size, info.st_mtime_ns, info.st_ctime_ns)


def sticky(method):
    """Only this finite owner's entry points: a caught refusal cannot enable retry."""
    @wraps(method)
    def call(self, *args, **kwargs):
        try:
            return method(self, *args, **kwargs)
        except BaseException as error:
            self._failure(error)
            raise
    return call


@dataclass(frozen=True)
class RawResult:
    stdout: bytes
    stderr: bytes
    exit_code: int
    evidence_json: str


@dataclass(frozen=True)
class FrameObservation:
    geometry: tuple
    renderer_observed: str | None
    theme_observed: str | None
    app_scale_configured: float | None
    evidence_json: str

    def to_evidence(self):
        return json.loads(self.evidence_json)


@dataclass(eq=False)
class _Binding:
    owner: object
    generation: str
    arm: str
    fixture: object
    ui_pid: int
    xid: int
    node: object
    ui_environment: dict
    root: Path
    closed: bool = False


class NativeHistoryOwner:
    """A source library, not a User/resource admission mechanism.

    sources[arm] must describe the exact worktree, frozen driver source,
    source_files hashes, actual matched CLI/HOST/UI paths+hashes, dev-runtime
    profile and owned manifest/index paths. tools contains absolute path/hash
    expectations. The final entry owes original build/data provenance; this
    library additionally opens, hashes and retains the actual named resources.
    Missing expectations refuse, never trigger discovery or repair.
    """

    def __init__(self, custody, cancelled, *, phase_deadline, whole_deadline,
                 root, sources, tools, entry_function=None, entry_source=None):
        self.custody, self.cancelled = custody, cancelled
        self.phase_deadline, self.whole_deadline = phase_deadline, whole_deadline
        self.sources, self.tools = sources, tools
        self.entry_function, self.entry_source = entry_function, entry_source
        self.root = Path(root)
        self.thread = threading.get_ident()
        self.events, self.files, self.direct_waits, self.services = [], [], {}, {}
        self.raw_fds, self._directories, self.launches = [], [], {}
        self.inputs = {}
        self.bindings, self.artifacts, self.producers, self.writer_fences = [], {}, {}, {}
        self.first_failure = None
        self.desktop = None
        self.desktop_attempted = self.settlement_attempted = False
        self.pidfd_close_attempted = False
        self.root_fd = self.root_record = self.ledger = None
        self._entry_guard()
        require(custody.cancelled is cancelled, "different cancellation owner")
        source = Path(inspect.getsourcefile(type(custody)))
        require(digest(source.read_bytes()) == CUSTODY_SHA, "unmatched custody implementation")
        require(type(custody).__name__ == "Custody" and not custody.closed, "custody is not open")
        require(math.isfinite(phase_deadline) and math.isfinite(whole_deadline)
                and phase_deadline <= whole_deadline, "invalid original deadlines")
        self._remaining()
        require(re.fullmatch(r"/tmp/cagents-history-[0-9a-f-]{36}", str(self.root)) is not None,
                "root must be the separately admitted fresh UUID directory")
        require(str(uuid.UUID(self.root.name.removeprefix("cagents-history-")))
                == self.root.name.removeprefix("cagents-history-"), "root UUID is not canonical")
        self._base_environment = os.environ.copy()
        require(not any(key in self._base_environment for key in ENDPOINT_KEYS + UNSUPPORTED_ENDPOINT_KEYS)
                and self._base_environment.get("XDG_SESSION_TYPE") != "wayland",
                "inherited native endpoint refused unchanged")
        routing = ("GIT_DIR", "GIT_WORK_TREE", "GIT_COMMON_DIR", "GIT_INDEX_FILE", "GIT_NAMESPACE",
                   "GIT_OBJECT_DIRECTORY", "GIT_ALTERNATE_OBJECT_DIRECTORIES", "GIT_CONFIG_COUNT",
                   "GIT_CONFIG_PARAMETERS", "GIT_CONFIG_SYSTEM", "GIT_CONFIG_GLOBAL", "GIT_REPLACE_REF_BASE")
        require(not any(key in self._base_environment for key in routing),
                "inherited Git routing refused unchanged")
        # Constructor acquires no FD or child: a preparation failure leaves an
        # already returned owner object holding every actual partial resource.

    def _entry_guard(self):
        require(self.entry_function is not None and self.entry_source is not None,
                "ORIGINAL_REVIEWED_ENTRY_UNSUPPLIED; standalone HOLD")
        path, expected = self.entry_source
        path = Path(path)
        info = path.lstat()
        require(path.is_absolute() and not path.is_symlink()
                and Path(self.entry_function.__code__.co_filename).resolve() == path.resolve()
                and stat.S_ISREG(info.st_mode) and info.st_size <= 1024 * 1024, "invalid entry source")
        if not hasattr(self, "_entry_pin"):
            require(digest(path.read_bytes()) == expected, "entry source provenance changed")
            self._entry_pin = unchanged(info)
        require(unchanged(path.lstat()) == self._entry_pin, "entry source identity/bytes metadata changed")
        frame = inspect.currentframe()
        try:
            require(any(value.frame.f_code is self.entry_function.__code__
                        for value in inspect.getouterframes(frame)), "original entry callsite absent")
        finally:
            del frame
        # This proves a source callsite only. The actual authenticated outer
        # instruction, natural caller and shared resource grant are external.

    def _remaining(self, cleanup=False):
        self._entry_guard()
        require(threading.get_ident() == self.thread, "different original owner thread")
        remaining = min(self.phase_deadline, self.whole_deadline) - time.monotonic()
        require(remaining > 0, "original absolute budget exhausted; retain custody")
        if not cleanup:
            require(not self.cancelled() and self.first_failure is None and not self.settlement_attempted,
                    "cancelled/failed owner refuses new work")
        return remaining

    def _failure(self, error):
        if getattr(self, "first_failure", None) is None:
            self.first_failure = {"type": type(error).__name__, "reason": str(error),
                                  "monotonic": time.monotonic()}
        if hasattr(self, "final_settlement"):
            self.final_settlement.update(first_failure=self.first_failure, disposition="HOLD")

    def _same_file(self, item):
        current = os.fstat(item["fd"])
        named = item["path"].lstat()
        require(stat.S_ISREG(current.st_mode) and stat.S_ISREG(named.st_mode)
                and identity(current) == identity(named) == item["identity"]
                and (current.st_nlink == 1 or "immutable" in item), "original file identity replaced")
        if "immutable" in item:
            require(unchanged(current) == unchanged(named) == item["immutable"], "immutable input changed")
        return current

    def _new_file(self, path, secret=False, cleanup=False):
        self._remaining(cleanup=cleanup)
        fd = os.open(path, os.O_RDWR | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW | os.O_CLOEXEC, 0o600)
        item = {"path": Path(path), "fd": fd,
                "secret": secret, "close_attempted": False}
        self.files.append(item)  # Actual open is retained BEFORE fallible fstat.
        self.events.append({"event": "file-open-return", "path": str(path), "fd": fd})
        item["identity"] = identity(os.fstat(fd))
        self.events.append({"event": "file-open-return", "path": str(path), "fd": fd,
                            "identity": list(item["identity"]), "secret_contents_not_recorded": secret})
        self._same_file(item)
        self._remaining(cleanup=cleanup)
        return item

    def _write(self, item, payload):
        self._remaining()
        self._same_file(item)
        view = memoryview(payload)
        while view:
            self._remaining()
            count = os.write(item["fd"], view)
            require(count > 0, "short evidence write")
            view = view[count:]
        os.fsync(item["fd"])
        self._same_file(item)
        self._remaining()

    def _close_file(self, item):
        require(not item["close_attempted"], "original close cannot be retried")
        if "identity" in item:
            item["before_close"] = unchanged(self._same_file(item))
        item["close_attempted"] = True
        try:
            os.close(item["fd"])
            item["close_returned"] = True
        except BaseException as error:
            item["close_returned"] = False
            self._failure(error)
            raise

    def _directory(self, parent, name):
        self._remaining()
        path = parent / name
        os.mkdir(path, 0o700)
        self.events.append({"event": "mkdir-return", "path": str(path)})
        fd = os.open(path, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW | os.O_CLOEXEC)
        record = self._retain_raw(fd, "directory:" + str(path))
        self.events.append({"event": "directory-open-return", "fd": fd, "path": str(path)})
        record.update(path=path, identity=identity(os.fstat(fd)))
        self._directories.append(record)
        require(record["identity"][2:4] == (os.getuid(), 0o700), "private directory mode/owner differs")
        self._remaining()
        return path

    def _profile(self, parent):
        return {key: str(self._directory(parent, key.lower())) for key in PRIVATE_KEYS}

    def _retain_raw(self, fd, role):
        item = {"fd": fd, "role": role, "acquisition": len(self.raw_fds), "close_attempted": False}
        self.raw_fds.append(item)  # Before fstat/readiness/post-fence; numeric FD reuse is legitimate.
        self.events.append({"event": "fd-acquisition-return", **item})
        return item

    def _pipe(self, role):
        read, write = os.pipe2(os.O_CLOEXEC | os.O_NONBLOCK)
        # Both actual return values are retained before another syscall.
        pair = [self._retain_raw(read, role + "-read"), self._retain_raw(write, role + "-write")]
        return pair

    def _pin_input(self, path, expected, maximum):
        self._remaining()
        path = Path(path)
        require(path.is_absolute() and path.resolve(strict=True) == path, "noncanonical pinned input")
        if str(path) in self.inputs:
            item = self.inputs[str(path)]
            self._same_file(item)
            require(item["sha256"] == expected, "conflicting immutable input expectation")
            return item
        fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_CLOEXEC)
        item = {"path": path, "fd": fd, "secret": False, "close_attempted": False}
        self.files.append(item)
        self.events.append({"event": "input-open-return", "path": str(path), "fd": fd})
        info = os.fstat(fd)
        item.update(identity=identity(info), immutable=unchanged(info))
        require(stat.S_ISREG(info.st_mode) and info.st_size <= maximum, "input is not a bounded regular file")
        hasher, offset = hashlib.sha256(), 0
        while offset < info.st_size:
            self._remaining()
            block = os.pread(fd, min(1024 * 1024, info.st_size - offset), offset)
            require(block, "immutable input shortened")
            hasher.update(block)
            offset += len(block)
        self._same_file(item)
        item["sha256"] = hasher.hexdigest()
        require(item["sha256"] == expected, "immutable input bytes differ")
        self.inputs[str(path)] = item
        self._remaining()
        return item

    def _tool(self, name):
        spec = self.tools[name]
        path = Path(spec["path"])
        require(path.is_absolute() and path.resolve(strict=True) == path,
                "tool must be an exact canonical path")
        self._pin_input(path, spec["sha256"], 1024 * 1024 * 1024)
        require(os.access(path, os.X_OK), "pinned tool is not executable")
        return str(path)

    def _launch(self, name, argv, environment, *, pass_fds=(), cleanup=False):
        self._remaining(cleanup=cleanup)
        number = len(self.custody.direct)
        stdout = self._new_file(self.root / f"process-{number}-{name}.stdout", cleanup=cleanup)
        stderr = self._new_file(self.root / f"process-{number}-{name}.stderr", cleanup=cleanup)
        try:
            child = self.custody.launch(argv, str(self.root), name, env=environment,
                                        stdout=stdout["fd"], stderr=stderr["fd"], pass_fds=pass_fds)
        except BaseException as error:
            # Custody.launch retained any successful Popen before fallible pin.
            # Preserve that original map; never replace a failed descriptor.
            self._failure(error)
            raise
        self.events.append({"event": "launch-return", "pid": child.pid, "role": name,
                            "argv": list(argv), "original_handle": id(child)})
        self.launches[child.pid] = {"child": child, "stdout": stdout, "stderr": stderr,
                                   "role": name, "argv": list(argv), "wait": None}
        stdout["writer_pid"] = stderr["writer_pid"] = child.pid
        self._remaining(cleanup=cleanup)
        return child, stdout, stderr

    def _wait(self, child, output, errors, limit, cleanup=False):
        cached_before = child.returncode
        while True:
            remaining = self._remaining(cleanup=cleanup)
            require(os.fstat(output["fd"]).st_size <= limit
                    and os.fstat(errors["fd"]).st_size <= limit, "tool output exceeded original bound")
            try:
                code = child.wait(timeout=min(remaining, .05))
                record = {"pid": child.pid, "original_Popen_wait_return": code,
                          "cached_status_before_wait": cached_before,
                          "kernel_wait_syscall_independently_observed": False,
                          "monotonic": time.monotonic()}
                self.direct_waits[child.pid] = record
                self.launches[child.pid]["wait"] = record
                self.events.append({"event": "original-wait-return", **record})
                self._remaining(cleanup=cleanup)
                break
            except subprocess.TimeoutExpired:
                continue
        self.custody.scan()
        require(os.fstat(output["fd"]).st_size <= limit
                and os.fstat(errors["fd"]).st_size <= limit, "late output exceeded original bound")
        os.lseek(output["fd"], 0, os.SEEK_SET)
        os.lseek(errors["fd"], 0, os.SEEK_SET)
        result = RawResult(os.read(output["fd"], limit + 1), os.read(errors["fd"], limit + 1),
                           code, json.dumps(record, sort_keys=True))
        require(len(result.stdout) <= limit and len(result.stderr) <= limit, "output overflow")
        self._remaining(cleanup=cleanup)
        return result

    def _capture(self, name, argv, environment, limit=MAX_OUTPUT, cleanup=False):
        child, output, errors = self._launch(name, argv, environment, cleanup=cleanup)
        try:
            result = self._wait(child, output, errors, limit, cleanup=cleanup)
        except BaseException as error:
            self._failure(error)
            raise  # Original handles and partial streams remain retained on HOLD.
        return result

    def _pipe_line(self, fd, child, cap=256):
        data = bytearray()
        poller = select.poll()
        poller.register(fd, select.POLLIN | select.POLLHUP | select.POLLERR)
        while True:
            remaining = self._remaining()
            require(child.returncode is None and not self.custody.exited(self.custody.nodes[child.pid]),
                    "owned server exited before readiness")
            if not poller.poll(max(1, int(min(remaining, .05) * 1000))):
                continue
            block = os.read(fd, cap + 1 - len(data))
            if not block:
                break
            data.extend(block)
            require(len(data) <= cap, "readiness byte limit")
        require(data.endswith(b"\n") and data.count(b"\n") == 1, "incomplete/duplicate readiness line")
        self._remaining()
        return bytes(data[:-1]).decode("ascii")

    def _socket_owner(self, path):
        """Exact owned pathname -> kernel listener -> already retained node.

        A numeric PID from a bus/property never creates or adopts a node.
        This is bounded original-family evidence, not global kernel closure.
        """
        self.custody.scan()
        rows = Path("/proc/net/unix").read_text().splitlines()[1:]
        matches = [row.split() for row in rows if len(row.split()) == 8 and row.split()[7] == str(path)]
        require(len(matches) == 1, "private listener pathname absent/ambiguous")
        require(matches[0][3] == "00010000" and matches[0][4] == "0001",
                "endpoint is not a kernel Unix stream listener")
        info = path.lstat()
        require(stat.S_ISSOCK(info.st_mode) and info.st_uid == os.getuid(), "listener path owner/type differs")
        inode = matches[0][6]
        owners = []
        for node in self.custody.nodes.values():
            if node.get("settled"):
                continue
            self.custody.current(node)
            descriptors = list(Path(f"/proc/{node['pid']}/fd").iterdir())
            require(len(descriptors) <= 4096, "owned descriptor bound")
            for descriptor in descriptors:
                try:
                    if os.readlink(descriptor) == f"socket:[{inode}]":
                        owners.append(node)
                        break
                except FileNotFoundError:
                    continue
        require(len(owners) == 1, "listener lacks one original-family owner")
        return owners[0], inode

    def _dbus(self, address, destination, path, method, *arguments):
        result = self._capture("private-dbus-client", [self._tool("gdbus"), "call", "--address", address,
            "--dest", destination, "--object-path", path, "--method", method, *arguments], self._environment)
        require(result.exit_code == 0, "private D-Bus request failed")
        return result.stdout.decode("utf-8", errors="strict").strip()

    @sticky
    def prepare_private_desktop(self):
        """Call before native imports; no outer xvfb-run/dbus-run-session."""
        self._remaining()
        require(not self.desktop_attempted, "private desktop cannot be replayed")
        self.desktop_attempted = True
        self.root_fd = os.open(self.root, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW | os.O_CLOEXEC)
        self.root_record = self._retain_raw(self.root_fd, "original-root-directory")
        self.root_identity = identity(os.fstat(self.root_fd))
        require(self.root_identity == identity(self.root.lstat())
                and self.root_identity[2:4] == (os.getuid(), 0o700)
                and not os.listdir(self.root_fd), "root is not fresh private state")
        self.ledger = self._new_file(self.root / "observations.jsonl")
        self._environment = {**self._base_environment, **self._profile(self.root),
                             "ICED_BACKEND": "wgpu", "WGPU_BACKEND": "gl", "LIBGL_ALWAYS_SOFTWARE": "1"}
        cookie = os.urandom(16)
        def auth(number):
            fields = (b"", number, b"MIT-MAGIC-COOKIE-1", cookie)
            return b"\xff\xff" + b"".join(len(field).to_bytes(2, "big") + field for field in fields)
        server = self._new_file(self.root / "server.Xauthority", secret=True)
        self._write(server, auth(b""))
        self._close_file(server)
        read, write = self._pipe("Xvfb-readiness")
        read_fd, write_fd = read["fd"], write["fd"]
        xvfb, xout, xerr = self._launch("owned-Xvfb", [self._tool("Xvfb"), "-displayfd", str(write_fd),
            "-screen", "0", "1600x1000x24", "-nolisten", "tcp", "-dpi", "96", "-auth", str(server["path"])],
            self._environment, pass_fds=(write_fd,))
        self.services["Xvfb"] = self.custody.nodes[xvfb.pid]
        self._close_raw(write)
        display = self._pipe_line(read_fd, xvfb)
        self._close_raw(read)
        require(re.fullmatch(r"[0-9]{1,5}", display), "invalid allocated display")
        client = self._new_file(self.root / "client.Xauthority", secret=True)
        self._write(client, auth(display.encode("ascii")))
        self._close_file(client)
        self._environment.update(DISPLAY=":" + display, XAUTHORITY=str(client["path"]))
        xnode, xinode = self._socket_owner(Path("/tmp/.X11-unix/X" + display))
        require(xnode is self.custody.nodes[xvfb.pid], "display listener differs from original Xvfb")
        bus_path = self.root / "session-bus"
        require(len(str(bus_path).encode()) < 100, "private bus path too long")
        read, write = self._pipe("session-readiness")
        read_fd, write_fd = read["fd"], write["fd"]
        bus, bout, berr = self._launch("owned-session-bus", [self._tool("dbus-daemon"), "--session", "--nofork",
            "--nopidfile", "--nosyslog", "--address=unix:path=" + str(bus_path), "--print-address=" + str(write_fd)],
            self._environment, pass_fds=(write_fd,))
        self.services["session"] = self.custody.nodes[bus.pid]
        self._close_raw(write)
        address = self._pipe_line(read_fd, bus)
        self._close_raw(read)
        require(re.fullmatch(re.escape("unix:path=" + str(bus_path)) + r",guid=[0-9a-f]{32}", address),
                "unexpected private session endpoint")
        bnode, binode = self._socket_owner(bus_path)
        require(bnode is self.custody.nodes[bus.pid], "session listener differs from original daemon")
        self._environment["DBUS_SESSION_BUS_ADDRESS"] = address
        # Standard bus activation, installed authentication/service descriptors
        # stay unchanged. Returned addresses/PIDs are corroboration, not owners.
        reply = self._dbus(address, "org.a11y.Bus", "/org/a11y/bus", "org.a11y.Bus.GetAddress")
        match = re.fullmatch(r"\('([^'\n]+)',\)", reply)
        require(match is not None, "invalid accessibility address response")
        ax_address = match.group(1)
        ax_match = re.fullmatch(r"unix:path=([^,]+),guid=[0-9a-f]{32}", ax_address)
        require(ax_match is not None and Path(ax_match.group(1)).is_relative_to(self.root),
                "accessibility bus escaped private root")
        ax_node, ax_inode = self._socket_owner(Path(ax_match.group(1)))
        self._pin_service(ax_node, "ax-bus", "dbus-daemon")
        launcher = self.custody.exact_matches(self._tool("at-spi-bus-launcher"))
        require(len(launcher) == 1, "accessibility launcher lacks unique original descendant")
        self._pin_service(launcher[0][0], "ax-launcher", "at-spi-bus-launcher")
        self._environment["AT_SPI_BUS_ADDRESS"] = ax_address
        self._dbus(ax_address, "org.freedesktop.DBus", "/org/freedesktop/DBus",
                   "org.freedesktop.DBus.StartServiceByName", "org.a11y.atspi.Registry", "0")
        pid_reply = self._dbus(ax_address, "org.freedesktop.DBus", "/org/freedesktop/DBus",
                              "org.freedesktop.DBus.GetConnectionUnixProcessID", "org.a11y.atspi.Registry")
        match = re.fullmatch(r"\(uint32 ([0-9]+),\)", pid_reply)
        require(match is not None, "registry PID response malformed")
        self.custody.scan()
        registry = self.custody.nodes.get(int(match.group(1)))
        require(registry is not None and not registry.get("settled"), "registry is not an owned descendant")
        self._pin_service(registry, "registry", "at-spi2-registryd")
        wm, wout, werr = self._launch("owned-window-manager", [self._tool("openbox"), "--sm-disable"], self._environment)
        self.services["wm"] = self.custody.nodes[wm.pid]
        self._pin_service(self.services["Xvfb"], "Xvfb", "Xvfb")
        self._pin_service(self.services["session"], "session", "dbus-daemon")
        self._pin_service(self.services["wm"], "wm", "openbox")
        # A running openbox PID is not a registered WM. Read only the private
        # root property, then corroborate its supporting window and original PID.
        while True:
            reply = self._capture("wm-root-property", [self._tool("xprop"), "-root", "_NET_SUPPORTING_WM_CHECK"], self._environment)
            match = re.fullmatch(rb"_NET_SUPPORTING_WM_CHECK\(WINDOW\): window id # (0x[0-9a-fA-F]+)\n?", reply.stdout)
            if reply.exit_code == 0 and match:
                wm_xid = int(match.group(1), 16)
                supporting = self._capture("wm-supporting-property", [self._tool("xprop"), "-id", hex(wm_xid), "_NET_SUPPORTING_WM_CHECK", "_NET_WM_PID"], self._environment)
                require(supporting.exit_code == 0 and re.fullmatch(
                    rb"_NET_SUPPORTING_WM_CHECK\(WINDOW\): window id # " + re.escape(hex(wm_xid).encode())
                    + rb"\n_NET_WM_PID\(CARDINAL\) = " + str(wm.pid).encode() + rb"\n?", supporting.stdout),
                    "WM registration differs from original window manager")
                break
            time.sleep(min(.05, self._remaining()))
        self.desktop = {"display": display, "session_address": address, "ax_address": ax_address,
                        "xnode": xnode, "x_inode": xinode, "busnode": bnode, "bus_inode": binode,
                        "axnode": ax_node, "ax_inode": ax_inode, "registry": registry}
        self.desktop["wm_xid"] = wm_xid
        self._remaining()
        # The reviewed entry may install ONLY these proved private endpoints
        # before native imports. Do not serialize the copied opaque environment.
        return {key: self._environment[key] for key in PRIVATE_KEYS + ENDPOINT_KEYS}

    def _pin_service(self, node, role, tool):
        current = self.custody.current(node, executable=True)
        require(current["executable"] == self._tool(tool), "private service executable differs: " + role)
        node["pinned_executable"] = current["executable"]
        node["pinned_executable_identity"] = current["executable_identity"]
        self.services[role] = node

    def _close_raw(self, item):
        require(not item["close_attempted"], "original FD acquisition close cannot be retried")
        item["close_attempted"] = True
        try:
            os.close(item["fd"])
            item["close_returned"] = True
            self.events.append({"event": "fd-close-return", "fd": item["fd"], "acquisition": item["acquisition"]})
        except BaseException as error:
            item["close_returned"] = False
            self._failure(error)
            raise

    def _source_inputs(self, arm, fixture):
        spec = self.sources[arm]
        worktree = Path(spec["worktree"])
        require(spec["profile"] == "dev-runtime", "wrong matched profile")
        git = self._tool("git")
        for args, expected in [(["rev-parse", "HEAD", "HEAD^{tree}"], "\n".join(ARMS[arm])),
                               (["status", "--porcelain=v1", "--untracked-files=all"], "")]:
            result = self._capture("source-git", [git, "--no-replace-objects", "--no-optional-locks", "-c",
                "core.hooksPath=/dev/null", "-c", "core.fsmonitor=false", "-C", str(worktree), *args], self._base_environment)
            require(result.exit_code == 0 and result.stdout.decode().strip() == expected, "source tuple/cleanliness mismatch")
        self._pin_input(worktree / "qualification-packets/pr1310-native-fixture/build.py", BUILDER_SHA, 1024 * 1024)
        require("scripts/run-development" in spec["source_files"]
                and "src/app/native_history_fixture.rs" in spec["source_files"], "runner/fixture source pins missing")
        for relative, expected in spec["source_files"].items():
            path = worktree / relative
            require(path.is_relative_to(worktree) and ".." not in Path(relative).parts, "source path escapes worktree")
            self._pin_input(path, expected, 64 * 1024 * 1024)
        manifest = self._open_input(Path(spec["manifest"]), fixture.manifest_sha256)
        index = None if fixture.index_sha256 is None else self._open_input(Path(spec["indexes"][fixture.dataset]), fixture.index_sha256)
        data_root = Path(spec["fixture_root"])
        require(data_root.is_absolute() and data_root.resolve(strict=True) == data_root
                and json.loads(manifest)["root"] == str(data_root)
                and Path(fixture.root).parent == data_root, "fixture root mismatch")
        root_info = data_root.lstat()
        require(stat.S_ISDIR(root_info.st_mode) and root_info.st_uid == os.getuid()
                and stat.S_IMODE(root_info.st_mode) == 0o700, "generated data root not private")
        repository = Path(fixture.root)
        for args, expected in [(["symbolic-ref", "--short", "HEAD"], "main")]:
            result = self._capture("fixture-git", [git, "--no-optional-locks", "-c", "core.hooksPath=/dev/null",
                "-C", str(repository), *args], self._base_environment)
            require(result.exit_code == 0 and result.stdout.decode().strip() == expected, "generated fixture branch mismatch")
        if fixture.head_oid is not None:
            for args, expected in [(["rev-parse", "HEAD"], fixture.head_oid),
                                   (["rev-list", "--count", "HEAD"], str(len(fixture.records))),
                                   (["rev-list", "--all", "--count"], str(len(fixture.records) + 1)),
                                   (["rev-parse", "refs/heads/sibling"], fixture.sibling["oid"])]:
                result = self._capture("fixture-git", [git, "--no-optional-locks", "-c", "core.hooksPath=/dev/null",
                    "-C", str(repository), *args], self._base_environment)
                require(result.exit_code == 0 and result.stdout.decode().strip() == expected, "generated Git identity/count mismatch")
        else:
            result = self._capture("fixture-git", [git, "--no-optional-locks", "-C", str(repository), "rev-parse", "--verify", "HEAD"], self._base_environment)
            require(result.exit_code != 0, "unborn fixture unexpectedly has HEAD")
        return spec, manifest, index

    def _open_input(self, path, expected):
        item = self._pin_input(path, expected, 64 * 1024 * 1024)
        size = self._same_file(item).st_size
        data = os.pread(item["fd"], size + 1, 0)
        require(len(data) == size and digest(data) == expected, "owned input changed during read")
        self._same_file(item)
        return data

    @sticky
    def require_history_root(self, arm, head, tree, fixture):
        self._remaining()
        require(self.desktop is not None and not any(not item.closed for item in self.bindings),
                "private desktop absent or previous UI still owned")
        require((head, tree) == ARMS.get(arm), "unknown adapter tuple")
        spec, manifest, index = self._source_inputs(arm, fixture)
        ui = spec["matched"]["ui"]
        # Root supplies actual separately built dev-runtime matched artifacts.
        # The NORMAL runner still performs its three serial build commands and
        # execs Cargo. Any subsequent replacement is a refusal, not a shortcut.
        for role in ("cli", "host", "ui"):
            self._pin_input(Path(spec["matched"][role]["path"]), spec["matched"][role]["sha256"], 2 * 1024 ** 3)
        root = self._directory(self.root, "generation-" + str(uuid.uuid4()))
        environment = {**self._environment, **self._profile(root),
                       "CAGENTS_DEVELOPMENT_HISTORY_FIXTURE": str(spec["manifest"]),
                       "CAGENTS_DEVELOPMENT_HISTORY_DATASET": fixture.dataset}
        require(self._base_environment.get("CARGO_BUILD_JOBS") == "2"
                and self._base_environment.get("CARGO_TARGET_DIR") == spec["cargo_target_dir"],
                "original admitted jobs/target context absent; never repair it")
        # No caller/authority/HOME/CODEX_HOME/PATH repair. The normal runner can
        # prepend its own verified Zig path and pre-main account handling stays.
        runner = Path(spec["worktree"]) / "scripts/run-development"
        log = self._new_file(root / "ui.log")
        child, stdout, stderr = self._launch("history-development-runner", [str(runner), "--browserless",
            "--debug", "--log-file", str(log["path"])], environment)
        while True:
            self._remaining()
            self.custody.scan()
            matches = self.custody.exact_matches(ui["path"])
            if len(matches) == 1:
                node, current = matches[0]
                require(digest(Path(f"/proc/{node['pid']}/exe").read_bytes()) == ui["sha256"], "live UI bytes differ")
                result = self._capture("window-discovery", [self._tool("xdotool"), "search", "--onlyvisible", "--pid", str(node["pid"])], environment)
                if result.exit_code == 0:
                    ids = result.stdout.decode().splitlines()
                    require(len(ids) == 1 and ids[0].isdigit(), "owned UI window absent/ambiguous")
                    xid = int(ids[0])
                    result = self._capture("window-PID", [self._tool("xdotool"), "getwindowpid", str(xid)], environment)
                    require(result.exit_code == 0 and result.stdout.strip() == str(node["pid"]).encode(), "window PID/property mismatch")
                    node["pinned_executable"] = ui["path"]
                    node["pinned_executable_identity"] = current["executable_identity"]
                    binding = _Binding(self, str(uuid.uuid4()), arm, fixture, node["pid"], xid, node, environment, root)
                    self.bindings.append(binding)
                    self._ui_log = log
                    self._runner = child
                    self._runner_streams = (stdout, stderr)
                    log["writer_pid"] = node["pid"]
                    self.check_history_root(binding, "initial-positive-window")
                    return binding
            require(child.returncode is None, "original runner exited before positive UI binding")
            time.sleep(min(.05, self._remaining()))

    @sticky
    def check_history_root(self, binding, operation):
        return self._verify_binding(binding, operation)

    def _verify_binding(self, binding, operation, cleanup=False):
        remaining = self._remaining(cleanup=cleanup)
        require(binding.owner is self and any(binding is item for item in self.bindings)
                and not binding.closed, "stale/foreign root generation")
        require(identity(os.fstat(self.root_fd))[:4] == identity(self.root.lstat())[:4] == self.root_identity[:4],
                "original private root replaced")
        for record in self._directories:
            require(identity(os.fstat(record["fd"]))[:4] == identity(record["path"].lstat())[:4]
                    == record["identity"][:4], "private directory replaced")
        current = self.custody.current(binding.node, executable=True)
        require(not self.custody.exited(binding.node) and current["state"] not in ("Z", "X"), "UI exited")
        require(current["executable"] == binding.node["pinned_executable"]
                and current["executable_identity"] == binding.node["pinned_executable_identity"], "UI executable changed")
        for node in (self.desktop["xnode"], self.desktop["busnode"], self.desktop["axnode"], self.desktop["registry"]):
            self.custody.current(node)
            require(not self.custody.exited(node), "private endpoint owner exited")
        for role, node in self.services.items():
            service_now = self.custody.current(node, executable=True)
            require(not self.custody.exited(node),
                    "private service exited: " + role)
            require(service_now["executable"] == node["pinned_executable"]
                    and service_now["executable_identity"] == node["pinned_executable_identity"],
                    "private service executable changed")
        for item in self.files:
            if not item["close_attempted"]:
                self._same_file(item)
            elif item.get("secret"):
                require(item.get("close_returned") is True
                        and unchanged(item["path"].lstat()) == item["before_close"], "sealed auth input changed")
        # Actual frozen after-save callsite, not a fabricated child writer wait.
        if operation in ("frame-WebP-before", "frame-WebP-after"):
            self._driver_fence(binding, operation)
        return self._remaining(cleanup=cleanup)

    def _driver_fence(self, binding, operation):
        spec = self.sources[binding.arm]["driver"]
        path = Path(spec["path"])
        require(digest(path.read_bytes()) == spec["sha256"], "driver source changed")
        frames = inspect.getouterframes(inspect.currentframe())
        try:
            matches = [item for item in frames if item.function == "capture"
                       and Path(item.filename).resolve() == path.resolve()
                       and getattr(item.frame.f_locals.get("self"), "binding", None) is binding]
            require(len(matches) == 1, "original driver capture callsite absent")
            local = matches[0].frame.f_locals
            require(isinstance(local.get("png"), Path) and local.get("kind") in ("screen", "drawable"),
                    "capture fence lacks original local artifact")
            png = str(local["png"])
            require(png in self.artifacts and self.artifacts[png]["binding"] is binding,
                    "capture fence artifact not originally reserved")
            previous = self.writer_fences.setdefault(binding.generation, [])
            if operation == "frame-WebP-after":
                require(previous and previous[-1]["operation"] == "frame-WebP-before"
                        and previous[-1]["png"] == png, "after-save lacks same original before fence")
                webp = str(local.get("webp"))
                require(webp in self.artifacts and self.artifacts[webp]["binding"] is binding,
                        "after-save output was not originally reserved")
            else:
                webp = None
            self.writer_fences.setdefault(binding.generation, []).append({"operation": operation,
                "png": png, "webp": webp, "kind": local["kind"],
                "thread": threading.get_ident(), "driver_sha256": spec["sha256"],
                "line": matches[0].lineno, "monotonic": time.monotonic()})
        finally:
            del frames

    @sticky
    def run_history_tool(self, binding, argv, output_limit):
        self.check_history_root(binding, "tool-admission")
        require(type(argv) is tuple and 0 < output_limit <= MAX_OUTPUT, "invalid finite tool request")
        require(argv and argv[0] in ("xdotool", "import"), "unplanned native tool")
        args = list(argv[1:])
        require(all(type(value) is str and "\0" not in value and len(value) <= 256 for value in args),
                "unbounded native argv")
        if argv[0] == "import":
            require(args and args in (["-screen", "-window", str(binding.xid), args[-1]],
                             ["-window", str(binding.xid), args[-1]]), "unplanned capture argv")
            path = str(Path(args[-1]))
            require(path in self.artifacts and self.artifacts[path]["binding"] is binding
                    and path.endswith(".png") and path not in self.producers, "unreserved/reused capture")
        else:
            fixed = (["getwindowfocus"], ["getwindowpid", str(binding.xid)],
                     ["getwindowgeometry", "--shell", str(binding.xid)],
                     ["windowmove", "--sync", str(binding.xid), "0", "0"])
            valid = args in fixed or (type(getattr(binding, "last_focus", None)) is int
                                      and args == ["getwindowpid", str(binding.last_focus)])
            valid |= len(args) == 5 and args[:3] == ["windowsize", "--sync", str(binding.xid)] and args[3:] in (["1440", "900"], ["900", "700"])
            valid |= len(args) == 2 and args[0] == "key" and args[1] in ("Return", "ctrl+a", "BackSpace", "Tab", "shift+Tab", "Escape")
            valid |= len(args) == 6 and args[:5] == ["type", "--clearmodifiers", "--delay", "1", "--"]
            if len(args) >= 4 and args[:2] == ["mousemove", "--sync"]:
                require(args[2].isdigit() and args[3].isdigit()
                        and 0 <= int(args[2]) < 1600 and 0 <= int(args[3]) < 1000, "pointer escaped private screen")
                tail = args[4:]
                valid |= tail in ([], ["click", "1"])
                valid |= (len(tail) == 6 and tail[:2] == ["click", "--repeat"] and tail[2].isdigit()
                          and 1 <= int(tail[2]) <= 25 and tail[3:5] == ["--delay", "20"] and tail[5] in ("4", "5"))
            require(valid, "unplanned fixed xdotool form")
        child, output, errors = self._launch("history-" + argv[0], [self._tool(argv[0]), *args], binding.ui_environment)
        if argv[0] == "import":
            self.producers[path] = {"argv": list(argv), "pid": child.pid, "result": None, "original_exit": None}
        result = self._wait(child, output, errors, output_limit)
        if argv[0] == "import":
            self.producers[path].update(result=json.loads(result.evidence_json), original_exit=result.exit_code)
        elif args == ["getwindowfocus"] and result.exit_code == 0:
            require(result.stdout.strip().isdigit(), "focus response is not a window id")
            binding.last_focus = int(result.stdout.strip())
        self.check_history_root(binding, "tool-return")
        return result

    @sticky
    def pause_history_poll(self, binding, maximum_seconds):
        remaining = self.check_history_root(binding, "poll-before")
        require(0 <= maximum_seconds <= .05, "unplanned poll delay")
        time.sleep(min(maximum_seconds, remaining))
        self.check_history_root(binding, "poll-after")

    @sticky
    def append_history_observation(self, binding, value):
        self.check_history_root(binding, "observation-before")
        record = {"version": 1, "generation": binding.generation, "ui_pid": binding.ui_pid,
                  "xid": binding.xid, "arm": binding.arm, "source": ARMS[binding.arm],
                  "manifest_sha256": binding.fixture.manifest_sha256,
                  "writer_pid": os.getpid(), "writer_thread": threading.get_ident(),
                  "monotonic": time.monotonic(), "observation": value}
        payload = (json.dumps(record, allow_nan=False, sort_keys=True) + "\n").encode()
        require(len(payload) <= 2 * 1024 * 1024, "observation exceeds bound")
        require(self._same_file(self.ledger).st_size + len(payload) <= 64 * 1024 * 1024, "total observation bound")
        self._write(self.ledger, payload)
        self.check_history_root(binding, "observation-after")

    @sticky
    def history_frame_observation(self, binding):
        self.check_history_root(binding, "frame-observation")
        result = self.run_history_tool(binding, ("xdotool", "getwindowgeometry", "--shell", str(binding.xid)), MAX_OUTPUT)
        require(result.exit_code == 0, "window geometry failed")
        fields = dict(line.split("=", 1) for line in result.stdout.decode().splitlines())
        require(int(fields["WINDOW"], 0) == binding.xid, "geometry XID changed")
        geometry = tuple(int(fields[name]) for name in ("X", "Y", "WIDTH", "HEIGHT"))
        # Real geometry/identity is observed. Fixture source configures dark/1.0,
        # but no actual renderer/theme observer is supplied by this component.
        # UNKNOWN must stop the driver's capture gate, never copy requested env.
        evidence = {"generation": binding.generation, "pid": binding.ui_pid, "xid": binding.xid,
                    "geometry": geometry, "requested_theme": "dark", "configured_fixture_scale": 1.0,
                    "renderer_observed": None, "theme_observed": None, "native_factor": None,
                    "reason": "actual renderer/theme observation producer remains UNSUPPLIED",
                    "ui_log_identity": list(self._ui_log["identity"])}
        return FrameObservation(geometry, None, None, 1.0, json.dumps(evidence, sort_keys=True))

    @sticky
    def reserve_history_artifact(self, binding, basename):
        self.check_history_root(binding, "artifact-reserve")
        require(re.fullmatch(r"[a-z0-9][a-z0-9-]{0,180}\.(png|webp)", basename), "unsafe artifact basename")
        path = binding.root / basename
        require(str(path) not in self.artifacts, "duplicate artifact allocation")
        item = self._new_file(path)
        self.artifacts[str(path)] = {"binding": binding, "item": item, "retained": False}
        return path

    @sticky
    def retain_history_artifact(self, binding, png, webp, kind, metrics):
        self.check_history_root(binding, "artifact-retain")
        require(kind in ("screen", "drawable"), "unknown frame kind")
        png, webp = Path(png), Path(webp)
        require(str(png) in self.artifacts and str(webp) in self.artifacts, "unreserved artifact")
        require(self.producers.get(str(png), {}).get("original_exit") == 0, "PNG producer did not succeed")
        fences = self.writer_fences.get(binding.generation, [])
        require(len(fences) >= 2 and [value["operation"] for value in fences[-2:]] ==
                ["frame-WebP-before", "frame-WebP-after"], "actual in-process save fence incomplete")
        require(all(value["png"] == str(png) and value["kind"] == kind for value in fences[-2:])
                and fences[-1]["webp"] == str(webp), "writer fence replay/mismatched pair")
        retained = []
        for path in (png, webp):
            record = self.artifacts[str(path)]
            require(record["binding"] is binding and not record["retained"], "artifact belongs to another generation/replay")
            item = record["item"]
            info = self._same_file(item)
            require(0 < info.st_size <= 32 * 1024 * 1024, "artifact empty/oversized")
            os.lseek(item["fd"], 0, os.SEEK_SET)
            payload = os.read(item["fd"], 32 * 1024 * 1024 + 1)
            require(len(payload) == info.st_size, "artifact changed during read")
            require(payload.startswith(b"\x89PNG\r\n\x1a\n") if path == png else
                    payload[:4] == b"RIFF" and payload[8:12] == b"WEBP", "unexpected original image format")
            self._same_file(item)
            record["retained"] = True
            retained.append({"path": str(path), "sha256": digest(payload), "bytes": len(payload),
                             "identity": list(item["identity"])})
        # Real synchronous image.save return is not a child wait or syscall
        # recording. Decoder/losslessness checks remain the actual driver/PIL
        # and independent review's obligation; this owner does not repair bytes.
        return {"originals": retained, "png_producer": self.producers[str(png)],
                "webp_writer": {"kind": "same-process-PIL-return", "source_fences": fences[-2:],
                                "invented_child_wait": False}, "metrics": metrics,
                "generation": binding.generation, "native_qualification": "NOT_ESTABLISHED_BY_RETENTION"}

    def record_history_failure(self, error):
        """Entry must latch driver/import/AX/decode failures before cleanup."""
        self._failure(error)

    @sticky
    def close_history_root(self, binding):
        self._verify_binding(binding, "normal-UI-close", cleanup=True)
        if self.cancelled():
            # Maintained launch refuses cancellation; do not replace its callback
            # to start a closing client. Only this positively held UI receives
            # TERM. This is forced failure cleanup, never a normal-close pass.
            self._failure(Hold("cancelled UI requires owned forced cleanup"))
            self.custody.signal_owned(binding.node, signal.SIGTERM)
        else:
            tool = self.inputs[str(Path(self.tools["xdotool"]["path"]))]
            self._same_file(tool)
            result = self._capture("normal-UI-close", [str(tool["path"]), "windowclose", str(binding.xid)],
                                   binding.ui_environment, cleanup=True)
            require(result.exit_code == 0, "normal owned window close failed")
        result = self._wait(self._runner, *self._runner_streams, 16 * 1024 * 1024, cleanup=True)
        self.custody.scan()
        require(self.custody.exited(binding.node), "UI closure lacks original pidfd exit")
        binding.closed = True
        self.events.append({"event": "UI-root-exited", "generation": binding.generation,
                            "runner_wait": json.loads(result.evidence_json),
                            "ui_node": {key: value for key, value in binding.node.items() if key != "pidfd"},
                            "runner_is_UI": self._runner.pid == binding.ui_pid})
        # Cargo's original wait and the UI's original pidfd are distinct. A UI
        # status reaped by Cargo can remain unknown in Custody; do not invent it.

    def _retain_failure_artifacts(self):
        records = []
        self.failure_artifacts = records
        for path, artifact in self.artifacts.items():
            self._remaining(cleanup=True)
            item = artifact["item"]
            info = self._same_file(item)
            record = {"path": path, "identity": list(item["identity"]), "bytes": info.st_size,
                      "accepted_pair": artifact["retained"], "sha256": None,
                      "png_producer": self.producers.get(path),
                      "save_fences": [fence for fences in self.writer_fences.values() for fence in fences
                                      if path in (fence["png"], fence["webp"])],
                      "frame_pass": False, "snapshot_only": True}
            records.append(record)  # Keep a partial record before fallible hash.
            if info.st_size <= 32 * 1024 * 1024:
                payload = os.pread(item["fd"], info.st_size + 1, 0)
                after = self._same_file(item)
                require(unchanged(info) == unchanged(after) and len(payload) == info.st_size,
                        "partial artifact changed during evidence retention")
                record["sha256"] = digest(payload)
            else:
                record["hash_gap"] = "raw output exceeds retained hash bound; original FD/path retained"
        self.events.append({"event": "raw-artifacts-retained-without-qualification", "artifacts": records})
        return records

    def _retain_and_close(self, item):
        if item["close_attempted"]:
            require(item.get("close_returned") is True, "unknown previous close; no retry")
            return
        self._remaining(cleanup=True)
        info = self._same_file(item)
        if not item.get("secret") and "immutable" not in item:
            require(info.st_size <= 64 * 1024 * 1024, "raw evidence exceeds close/hash bound")
            hasher, offset = hashlib.sha256(), 0
            while offset < info.st_size:
                self._remaining(cleanup=True)
                payload = os.pread(item["fd"], min(1024 * 1024, info.st_size - offset), offset)
                require(payload, "raw evidence shortened before closure")
                hasher.update(payload)
                offset += len(payload)
            require(unchanged(self._same_file(item)) == unchanged(info), "evidence writer not settled")
            self.events.append({"event": "raw-file-before-close", "path": str(item["path"]),
                                "bytes": info.st_size, "sha256": hasher.hexdigest(),
                                "identity": list(item["identity"])})
        self._close_file(item)

    @sticky
    def settle_history_services(self):
        """Finite role-ordered TERM proposal; no automatic logical-C handback.

        UI/reader/driver writers must finish first. Unknown original waits,
        arbitrary descendants, raw nonservice signals, failure or FD ambiguity
        remain HOLD. Raw maintained signal history is never relabeled/cleared.
        """
        self._remaining(cleanup=True)
        require(not self.settlement_attempted, "settlement/close attempts are not replayable")
        self.settlement_attempted = True
        require(all(item.closed for item in self.bindings), "close owned UIs before desktop services")
        self._retain_failure_artifacts()  # Includes PNG-only alpha/decode failures.
        self.custody.scan()
        service_ids = {node["pid"] for node in self.services.values()}
        require(set(self.custody.direct).issubset(self.launches), "partial launch is retained but lacks owner-role receipt")
        nonservices = [node for node in self.custody.nodes.values() if node["pid"] not in service_ids]
        require(all(node.get("settled") for node in nonservices), "UI/Git/tool descendants still live; keep buses")
        for pid, original in self.launches.items():
            if pid not in service_ids:
                # Even if Custody.poll reaped earlier, record the actual original
                # Popen.wait return and its cached-status provenance.
                if pid not in self.direct_waits:
                    self._wait(original["child"], original["stdout"], original["stderr"],
                               16 * 1024 * 1024, cleanup=True)
        # No driver/evidence writer outlives these closures. Service streams may
        # also be inherited by activated AX descendants, so keep ALL of those
        # files until every private service has positively settled.
        for item in self.files:
            if item.get("writer_pid") not in service_ids:
                self._retain_and_close(item)
        for role in ("registry", "ax-bus", "ax-launcher", "wm", "session", "Xvfb"):
            node = self.services.get(role)
            if node is None:
                continue
            self._remaining(cleanup=True)
            self.custody.scan()
            if not self.custody.exited(node):
                self.custody.signal_owned(node, signal.SIGTERM)
            original = self.launches.get(node["pid"])
            if original is not None:
                self._wait(original["child"], original["stdout"], original["stderr"],
                           16 * 1024 * 1024, cleanup=True)
            else:
                while not self.custody.reap(node):
                    time.sleep(min(.05, self._remaining(cleanup=True)))
                    self.custody.scan()
            self.events.append({"event": "private-service-settled", "role": role,
                                "node": {key: value for key, value in node.items() if key != "pidfd"}})
        snapshot = self.custody.snapshot()
        unknown = [node for node in snapshot["nodes"] if not node.get("settled") or node.get("exit_code") is None]
        raw_signals = [{"pid": node["pid"], **item} for node in snapshot["nodes"] for item in node.get("signals", [])]
        self.final_settlement = {"custody": snapshot, "direct_waits": dict(self.direct_waits),
            "raw_signals_unchanged": raw_signals, "unknown": unknown,
            "first_failure": self.first_failure, "failure_artifacts": self.failure_artifacts,
            "planned_service_TERM_policy": "PROPOSED_ONLY_REQUIRES_FRESH_ACCEPTANCE",
            "logical_C_return": "NOT_AUTHORIZED_BY_THIS_LIBRARY", "disposition": "HOLD"}
        # Preserve the snapshot even when these gates refuse final descriptor
        # closure. A later empty census or successful close cannot erase it.
        require(not unknown, "unknown status/custody; keep original descriptors and logical HOLD")
        require(all(item["signal"] == signal.SIGTERM and item["pid"] in service_ids for item in raw_signals),
                "forced/nonservice signal retained; logical HOLD")
        for item in self.files:
            if not item["close_attempted"]:
                self._retain_and_close(item)
        require(not self.pidfd_close_attempted, "pidfd closure cannot be retried")
        self.pidfd_close_attempted = True
        self.custody.close_pidfds()  # Primitive pops before close: ambiguity => no retry/HOLD.
        self.final_settlement["pidfd_close_returned"] = True
        for item in reversed(self.raw_fds):
            if not item["close_attempted"]:
                self._remaining(cleanup=True)
                self._close_raw(item)
        self.final_settlement["raw_fd_closes_returned"] = True
        self.final_settlement["disposition"] = "SOURCE_PREDICATES_ONLY_NOT_RESOURCE_RELEASE"
        # first_failure stays sticky even after all physical closes succeed.
        if self.first_failure is not None:
            self.final_settlement["disposition"] = "HOLD_ORIGINAL_FAILURE_RETAINED"
        return self.final_settlement


if __name__ == "__main__":
    raise SystemExit("ORIGINAL_ENTRY_UNSUPPLIED: source library only; no execution admission")
