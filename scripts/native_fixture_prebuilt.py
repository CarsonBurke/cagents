"""Fixed a48 package support for the person-started native fixture broker.

Imported only after the broker's original User guard. No Agent request selects
an executable, environment, socket, profile or package. Actual native admission
and recovery assertions are separate from these source/mock contracts.
"""
import hashlib
import json
import os
from pathlib import Path
import re
import select
import signal
import socket
import stat
import subprocess
import sys
import time

SOURCE = "a48d0f75eca3e48eac42bcb2955df53b2ec6ab9f"
PACKAGE = Path("/var/tmp/coord-b9-a48-isolated-8a_xjx4x/package")
PINS = {
    "cagents": ("d19be23b8a728fc431be58329ef8684a9eb936c34a2e1c7f24ad536da80908d5", 5294920, 0o755),
    "cagentsd": ("503b6bd6936dd1c3fc893dac3d787c3579830336b3c18fb9ec6bf31ca2b5a3a7", 26338216, 0o755),
    "cagents-ui": ("d814e7beb78037649734250217c6c924703f1ee44f30f89a60604d244dddcc50", 71688144, 0o755),
    ".cagents-ui-lease-v1": ("e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855", 0, 0o644),
}
PROVIDER_SHA256 = "a7c31648b41089d6b0919b4245ce1eef4b896ff932ab8cd41de9859fa4ce29f6"
MOUNT = "/opt/cagents-a48"
STEPS = ("daemon-start", "ready", "daemon-status", "preflight", "ui-start", "ui-wait",
         "empty-stop", "checkpoint", "daemon-wait")
FIXED_CLI = {
    "ready": ["start"],
    "daemon-status": ["status"], "preflight": ["preflight"],
    "empty-stop": ["stop", "--require-empty"],
    "checkpoint": ["prepare-upgrade", "--yes", "--no-relaunch-agents"],
}
# No session flag, host display, host bus, default config or global environment
# import. KWin's virtual framebuffer is the parent of this original niri only.
NIRI_CONFIG = 'input { keyboard { xkb { layout "us"; }; }; }\nprefer-no-csd\n'
HANDSHAKE = r'''
import json, os, pathlib
p = pathlib.Path(__import__('sys').argv[1])
pending = p.with_name('handshake.pending')
f = os.open(pending, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
with os.fdopen(f, 'w') as out:
    json.dump({'display': os.environ['WAYLAND_DISPLAY'], 'socket': os.environ['NIRI_SOCKET']}, out)
# Publish only a complete closed record, atomically and without overwriting.
os.link(pending, p)
os.unlink(pending)
'''


def environment():
    """Private User-owned sandbox context; never derived from Agent requests."""
    return {"HOME": "/fixture/home", "USER": "fixture", "LOGNAME": "fixture",
            "PATH": "/fixture/bin:/usr/bin:/bin", "LANG": "C.UTF-8", "TERM": "dumb",
            "TMPDIR": "/tmp", "XDG_RUNTIME_DIR": "/run/cagents-fixture",
            "XDG_DATA_HOME": "/fixture/data", "XDG_CONFIG_HOME": "/fixture/config",
            "XDG_CACHE_HOME": "/fixture/cache", "XDG_STATE_HOME": "/fixture/state",
            "XDG_SESSION_TYPE": "wayland", "WAYLAND_DISPLAY": "nested-wayland"}


class Package:
    """A held fixed directory; hash/type/mode/identity checked before each use."""
    def __init__(self, base):
        self.base = base
        self.first_files = {}
        self.fd = base.hold_directory(Path("/"), PACKAGE.parts[1:])
        try:
            self.identity = os.fstat(self.fd)
            self.verify()
        except BaseException:
            os.close(self.fd)
            self.fd = None
            raise

    def verify(self):
        b = self.base
        if self.fd is None:
            raise b.Refusal("package_changed", "original package directory is closed")
        current = os.fstat(self.fd)
        named = os.stat(PACKAGE, follow_symlinks=False)
        if ((current.st_dev, current.st_ino) != (named.st_dev, named.st_ino)
                or current.st_uid != os.geteuid() or current.st_mode & 0o022):
            raise b.Refusal("package_changed", "fixed package directory changed or is not privately controlled")
        for name, (wanted, size, mode) in PINS.items():
            fd = os.open(name, os.O_RDONLY | os.O_NONBLOCK | os.O_NOFOLLOW | os.O_CLOEXEC, dir_fd=self.fd)
            try:
                before = os.fstat(fd)
                if (not stat.S_ISREG(before.st_mode) or before.st_uid != os.geteuid()
                        or before.st_nlink != 1 or stat.S_IMODE(before.st_mode) != mode or before.st_size != size):
                    raise b.Refusal("package_changed", "fixed package companion has wrong type, ownership, mode or size")
                digest = hashlib.sha256()
                for _ in range((size // (1024 * 1024)) + 2):
                    block = os.read(fd, 1024 * 1024)
                    if not block:
                        break
                    digest.update(block)
                else:
                    raise b.Refusal("package_changed", "fixed package grew during verification")
                after = os.fstat(fd)
                named_file = os.stat(name, dir_fd=self.fd, follow_symlinks=False)
                fields = lambda st: (st.st_dev, st.st_ino, st.st_size, st.st_mtime_ns, st.st_ctime_ns)
                if (fields(before) != fields(after) or fields(after) != fields(named_file)
                        or (name in self.first_files and self.first_files[name] != fields(after))
                        or digest.hexdigest() != wanted):
                    raise b.Refusal("package_changed", "fixed package companion digest or identity changed")
                self.first_files.setdefault(name, fields(after))
            finally:
                os.close(fd)

    def close(self):
        fd, self.fd = self.fd, None
        if fd is not None:
            os.close(fd)  # Never retry an uncertain descriptor close.


# Trusted relay keeps the ORIGINAL Popen and capture owners in the private PID
# namespace. No subprocess.run timeout-kill, guessed PID, signal or raw argv API.
# EOF is not cleanup: a lost broker retains live children/unknown descendants.
RELAY = r'''
import json, os, pathlib, socket, stat, struct, subprocess, sys, time
ENV = %r
CLI = %r
ROOT = pathlib.Path('/fixture/evidence')
children = {}
history = []
sequence = 0
failed = False
closing = False
ready_record = None
adopted_exits = []

def state(record):
    global failed
    child = record['child']
    code = child.poll()
    if code is not None and record['role'] in ('daemon', 'ui') and code != 0:
        failed = True
    if code is not None and not record['settled']:
        child.wait()
        record['settled'] = True
        for stream in record['streams']:
            if id(stream) in record['closed_streams']: continue
            record['closed_streams'].add(id(stream))
            try:
                stream.close()
            except BaseException:
                record['close_unknown'] = True
    return {'state': 'running' if code is None else 'exited', 'exit_code': code,
            'original_wait': record['settled'], 'close_unknown': record['close_unknown']}

def spawn(role, argv):
    global sequence
    if role in children and not state(children[role])['original_wait']:
        raise ValueError('original child remains active')
    sequence += 1
    streams = []
    record = {'child': None, 'streams': streams, 'settled': False, 'close_unknown': False, 'closed_streams': set(), 'raw_fds': [], 'role': role, 'spawn_unknown': False}
    history.append(record)
    # Register each resource before the next fallible acquisition.
    children[role] = record
    try:
        for suffix in ('out', 'err'):
            fd = os.open(ROOT / (str(sequence) + '-' + role + '.' + suffix),
                         os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
            record['raw_fds'].append(fd)
            try:
                stream = os.fdopen(fd, 'wb')
            except BaseException:
                record['raw_fds'].remove(fd)
                try: os.close(fd)
                except BaseException: record['close_unknown'] = True
                raise
            record['raw_fds'].remove(fd)
            streams.append(stream)
        record['child'] = subprocess.Popen(argv, env=ENV, cwd='/fixture/project',
            stdin=subprocess.DEVNULL, stdout=streams[0], stderr=streams[1], start_new_session=True)
    except BaseException as original:
        if not isinstance(original, (OSError, ValueError)) and len(streams) == 2:
            record['spawn_unknown'] = True
        for stream in streams:
            if id(stream) in record['closed_streams']: continue
            record['closed_streams'].add(id(stream))
            try: stream.close()
            except BaseException: record['close_unknown'] = True
        if not record['close_unknown'] and not record['spawn_unknown']:
            record['settled'] = True
            del children[role]
        raise
    return record

def states():
    return {str(index) + '-' + record['role']: state(record) if record['child'] is not None else
            {'state': 'not_started' if record['settled'] else 'unknown', 'original_wait': record['settled'],
             'close_unknown': record['close_unknown'] or record['spawn_unknown']}
            for index, record in enumerate(history)}

def settled(role):
    return role not in children or state(children[role])['original_wait']

def reap_exited():
    # --as-pid-1 makes this relay the private namespace's reaper. WNOWAIT is
    # kernel proof of an EXITED direct child, never live-PID/control authority.
    # Every original Popen generation keeps its own wait; historic settled PIDs
    # are not excluded because a later adopted child may reuse their number.
    if any(not row['settled'] for row in adopted_exits): return False
    deadline = time.monotonic() + .5
    for _ in range(128):
        if time.monotonic() >= deadline: return False
        try: peek = os.waitid(os.P_ALL, 0, os.WEXITED | os.WNOHANG | os.WNOWAIT)
        except ChildProcessError: return True
        if peek is None: return True
        original = next((record for record in history if record['child'] is not None
                         and not record['settled'] and record['child'].pid == peek.si_pid), None)
        if original is not None:
            state(original)
            if not original['settled']: return False
            continue
        receipt = {'pid': peek.si_pid, 'uid': peek.si_uid, 'code': peek.si_code,
                   'status': peek.si_status, 'settled': False, 'waitpid': None, 'error': None}
        adopted_exits.append(receipt)  # Retain original kernel receipt BEFORE consuming it.
        try:
            observed = os.waitpid(peek.si_pid, os.WNOHANG)
            receipt['waitpid'] = list(observed)
            receipt['settled'] = observed[0] == peek.si_pid
        except BaseException as error:
            receipt['error'] = type(error).__name__
        if not receipt['settled']: return False
    return False

def owned_endpoint():
    child = children.get('daemon', {}).get('child')
    if child is None or child.poll() is not None: return False
    root = pathlib.Path('/run/cagents-fixture/cagents')
    try:
        directory = root.lstat()
        endpoint = (root / 'cagentsd.sock').lstat()
        lock = (root / 'cagentsd.lock').lstat()
        if (not stat.S_ISDIR(directory.st_mode) or directory.st_uid != os.geteuid()
                or directory.st_mode & 0o077 or not stat.S_ISSOCK(endpoint.st_mode)
                or endpoint.st_uid != os.geteuid() or endpoint.st_mode & 0o077
                or not stat.S_ISREG(lock.st_mode) or lock.st_nlink != 1
                or lock.st_uid != os.geteuid() or lock.st_mode & 0o077): return False
        with socket.socket(socket.AF_UNIX) as probe:
            probe.settimeout(.2)
            probe.connect(str(root / 'cagentsd.sock'))
            pid, uid, gid = struct.unpack('3i', probe.getsockopt(socket.SOL_SOCKET, socket.SO_PEERCRED, 12))
        return pid == child.pid and uid == os.geteuid()
    except OSError: return False

def run(request):
    global failed, closing, ready_record
    action = request.get('action')
    snapshot = states()
    reaped = reap_exited()
    if action == 'status':
        return {'children': snapshot, 'failed': failed, 'adopted_wait_count': len(adopted_exits), 'wait_hold': not reaped}
    if not reaped:
        raise ValueError('exited adopted-child wait remains unknown or bounded reap incomplete')
    if any(row['close_unknown'] for row in snapshot.values()):
        raise ValueError('original capture close remains unknown')
    if any(not record['settled'] for record in history if record['role'].startswith('control')):
        raise ValueError('original control child has not settled')
    if action == 'shutdown':
        if any(not row['original_wait'] for row in snapshot.values()):
            raise ValueError('original UI/daemon child has not settled')
        remaining = [name for name in os.listdir('/proc') if name.isdecimal() and int(name) != os.getpid()]
        if remaining:
            raise ValueError('private namespace still has retained or unknown processes')
        closing = True
        return {'settled': True, 'original_children': snapshot, 'adopted_waits': adopted_exits, 'remaining': 0}
    if failed and action not in ('ui-wait', 'daemon-wait', 'empty-stop'):
        raise ValueError('original negative retained; cleanup/wait only')
    if action in ('daemon-start', 'ui-start'):
        if not settled('ui'):
            raise ValueError('original UI/window writers have not settled')
        if action == 'daemon-start':
            if not settled('daemon'):
                raise ValueError('original daemon has not settled')
            if os.path.lexists('/run/cagents-fixture/cagents/cagentsd.sock'):
                raise ValueError('unexpected socket before original daemon start')
            ready_record = None
            record = spawn('daemon', ['/opt/cagents-a48/cagentsd'])
            deadline = time.monotonic() + 10
            while time.monotonic() < deadline:
                if owned_endpoint(): break
                if record['child'].poll() is not None: raise ValueError('original daemon exited before readiness')
                time.sleep(.05)
            else: raise ValueError('original daemon readiness unknown')
        else:
            if settled('daemon') or not owned_endpoint() or ready_record is not children['daemon']:
                raise ValueError('no successful readiness receipt for this exact daemon generation')
            record = spawn('ui', ['/opt/cagents-a48/cagents-ui', '--device', 'local', '--window', 'a48-proof'])
        return {'started': action, 'original_wait': False}
    if action in ('ui-wait', 'daemon-wait'):
        role = action.removesuffix('-wait')
        if role not in children:
            raise ValueError('no original child was started')
        try: children[role]['child'].wait(timeout=10)
        except subprocess.TimeoutExpired: return {'settled': False, 'children': states()}
        return {'settled': True, 'children': states(), 'failed': failed}
    if action in CLI:
        if settled('daemon') or not owned_endpoint():
            raise ValueError('no exact owned live daemon endpoint')
        if action in ('empty-stop', 'checkpoint') and not settled('ui'):
            raise ValueError('close the normal WINDOW and wait for its original Child first')
        record = spawn('control-' + str(sequence + 1),
            ['/opt/cagents-a48/cagents', '--device', 'local', '--no-start', '--json', 'daemon'] + CLI[action])
        try: code = record['child'].wait(timeout=30)
        except subprocess.TimeoutExpired:
            failed = True
            return {'settled': False, 'failed': True}
        row = state(record)
        if code != 0: failed = True
        if action == 'ready' and code == 0:
            if not owned_endpoint(): raise ValueError('original daemon endpoint changed after readiness')
            ready_record = children['daemon']
        # Raw stdout stays only in the private evidence capture: preflight can
        # contain a capability. No value or private descriptor hash is returned.
        return {'settled': row['original_wait'], 'exit_code': code, 'failed': failed}
    raise ValueError('unknown fixed action')

channel = socket.socket(fileno=int(sys.argv[1]))
try:
    with channel.makefile('rwb') as stream:
        stream.write(b'{"id":0,"ready":true}\n'); stream.flush()
        for line in stream:
            request = json.loads(line)
            try: reply = run(request)
            except Exception as error:
                failed = True
                reply = {'refused': str(error), 'failed': True}
            reply['id'] = request['id']
            stream.write(json.dumps(reply).encode() + b'\n'); stream.flush()
            if closing: break
finally:
    # Broken IO/malformed requests/cancellation do not discard live originals.
    while not closing:
        try:
            snapshot = states()
            if reap_exited() and all(row['original_wait'] and not row['close_unknown'] for row in snapshot.values()):
                remaining = [p for p in os.listdir('/proc') if p.isdecimal() and int(p) != os.getpid()]
                if not remaining: break
        except BaseException:
            pass
        try: time.sleep(.1)
        except BaseException: pass
''' % (environment(), FIXED_CLI)


class Desktop:
    """Original private bus/KWin/niri and command owners; no numeric adoption."""
    def __init__(self, base, layout):
        self.base, self.layout = base, layout
        self.records = []
        self.children = {}
        self.env = {"HOME": str(layout.private / "home"), "PATH": "/usr/bin:/bin",
                    "LANG": "C.UTF-8", "XDG_RUNTIME_DIR": str(layout.niri_root),
                    "XDG_CONFIG_HOME": str(layout.private / "config"),
                    "XDG_DATA_HOME": str(layout.private / "data"),
                    "XDG_CACHE_HOME": str(layout.private / "cache")}
        self.display = self.ipc = None
        self.ipc_fd = None
        self.ipc_close_unknown = False
        self.quit_attempted = False
        self.quit_ack = False
        self.termination_attempted = set()

    def spawn(self, role, argv):
        record = {"role": role, "child": None, "streams": [], "waited": False, "close_unknown": False, "closed_streams": set(), "spawn_unknown": False, "raw_fds": []}
        self.records.append(record)
        for suffix in ("out", "err"):
            fd = os.open(self.layout.private / f"{len(self.records)}-{role}.{suffix}",
                         os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
            record["raw_fds"].append(fd)
            try:
                stream = os.fdopen(fd, "wb")
            except BaseException:
                record["raw_fds"].remove(fd)
                try: os.close(fd)
                except BaseException: record["close_unknown"] = True
                raise
            record["raw_fds"].remove(fd)
            record["streams"].append(stream)
        try:
            record["child"] = subprocess.Popen(argv, env=self.env, stdin=subprocess.DEVNULL,
                stdout=record["streams"][0], stderr=record["streams"][1], start_new_session=True, umask=0o077)
        except OSError:
            raise  # Popen reports a definite exec/create failure.
        except BaseException:
            record["spawn_unknown"] = True
            raise
        return record

    def wait(self, record, timeout):
        if record.get("spawn_unknown"):
            raise self.base.Refusal("cleanup_incomplete", "original display spawn disposition is unknown")
        if record["child"] is None:
            # Failed acquisition is not a live Child; captures still require close.
            record["waited"] = True
            code = None
        else:
            code = record["child"].wait(timeout=timeout)
            record["waited"] = True
        for stream in record["streams"]:
            if id(stream) in record["closed_streams"]:
                continue
            record["closed_streams"].add(id(stream))
            try: stream.close()
            except BaseException: record["close_unknown"] = True
        if record["close_unknown"]:
            raise self.base.Refusal("cleanup_incomplete", "original display capture close is unknown")
        return code

    def command(self, argv, timeout=30):
        if self.ipc is not None:
            self.verify_ipc()
        record = self.spawn("command", argv)
        code = self.wait(record, timeout)
        # Paths are broker-private original captures, never sandbox outputs.
        def tail(suffix):
            path = self.layout.private / f"{self.records.index(record) + 1}-command.{suffix}"
            with path.open("rb") as stream:
                stream.seek(0, 2)
                stream.seek(max(0, stream.tell() - 65536))
                return stream.read(65536)
        return subprocess.CompletedProcess(argv, code, tail("out"), tail("err"))

    def until(self, predicate):
        deadline = time.monotonic() + 30
        while time.monotonic() < deadline:
            if any(r["child"].poll() is not None for r in self.children.values()):
                break
            if predicate(): return
            time.sleep(.05)
        raise self.base.Refusal("headless_failed", "original private display did not become ready")

    def socket_ok(self, path):
        try:
            metadata = path.lstat()
            return stat.S_ISSOCK(metadata.st_mode) and metadata.st_uid == os.geteuid() and not metadata.st_mode & 0o077
        except FileNotFoundError:
            return False

    def start(self):
        root = self.layout.private
        for name in ("home", "config", "data", "cache"):
            (root / name).mkdir(mode=0o700)
        self.children["bus"] = self.spawn("bus", ["/usr/bin/dbus-daemon", "--session", "--nofork", "--print-address=1",
            "--address=unix:path=" + str(self.layout.niri_root / "bus")])
        address_path = root / "1-bus.out"
        self.until(lambda: b"\n" in address_path.read_bytes())
        address = address_path.read_text().splitlines()[0]
        if not address.startswith("unix:"):
            raise self.base.Refusal("headless_failed", "private bus returned an unsupported address")
        self.env["DBUS_SESSION_BUS_ADDRESS"] = address
        self.children["kwin"] = self.spawn("kwin", ["/usr/bin/kwin_wayland", "--virtual", "--socket", "a48-parent",
            "--width", "1600", "--height", "1000", "--no-lockscreen", "--no-global-shortcuts", "--no-kactivities"])
        self.until(lambda: self.socket_ok(self.layout.niri_root / "a48-parent"))
        self.env["WAYLAND_DISPLAY"] = "a48-parent"
        config, handshake = root / "niri.kdl", root / "handshake.json"
        config.write_text(NIRI_CONFIG)
        self.children["niri"] = self.spawn("niri", [self.layout.host.niri, "-c", str(config), "--",
            "/usr/bin/python3", "-I", "-c", HANDSHAKE, str(handshake)])
        self.until(handshake.is_file)
        reply = json.loads(handshake.read_text())
        display, ipc = reply.get("display"), reply.get("socket")
        if (not isinstance(display, str) or not re.fullmatch(r"wayland-[0-9]+", display)
                or not isinstance(ipc, str) or Path(ipc).parent != self.layout.niri_root):
            raise self.base.Refusal("headless_failed", "private niri handshake paths refused")
        self.until(lambda: self.socket_ok(self.layout.niri_root / display) and self.socket_ok(Path(ipc)))
        self.display = display
        # Hold the verified socket inode. All later IPC routes through this
        # original descriptor, never a replaced pathname or rediscovered PID.
        self.ipc_fd = self.base.hold_socket(self.layout.niri_root, Path(ipc).name)
        self.ipc = f"/proc/{os.getpid()}/fd/{self.ipc_fd}"
        self.verify_ipc()
        self.env["WAYLAND_DISPLAY"], self.env["NIRI_SOCKET"] = display, self.ipc
        self.env["NIRI_HARNESS_STATE"] = str(self.layout.harness_state)
        # This record routes only existing input/capture. Original custody stays
        # in self.children, never in the handshake PID or harness session stop.
        record = {"name": self.layout.name, "display": display, "socket": self.ipc,
                  "pid": self.children["niri"]["child"].pid, "root": str(self.layout.niri_root)}
        (self.layout.harness_state / f"{self.layout.name}.json").write_text(json.dumps(record))
        return display

    def owned_connection(self):
        if self.ipc_fd is None or self.ipc_close_unknown:
            raise self.base.Refusal("cleanup_incomplete", "original niri IPC descriptor unavailable")
        original = self.children["niri"]["child"]
        if original.poll() is not None:
            raise self.base.Refusal("headless_failed", "original niri exited")
        import struct
        probe = socket.socket(socket.AF_UNIX)
        try:
            probe.settimeout(1)
            probe.connect(self.ipc)
            pid, uid, _gid = struct.unpack("3i", probe.getsockopt(socket.SOL_SOCKET, socket.SO_PEERCRED, 12))
            if pid != original.pid or uid != os.geteuid():
                raise self.base.Refusal("headless_failed", "private niri IPC is not the original Child")
            return probe
        except BaseException:
            probe.close()
            raise

    def verify_ipc(self):
        with self.owned_connection():
            pass

    def quit_original(self):
        # niri v26.04 niri-ipc Request::Action(Action::Quit) and Reply::Ok(Handled).
        # https://github.com/YaLTeR/niri/blob/v26.04/niri-ipc/src/lib.rs
        # Do not retry an uncertain request or accept EOF as the acknowledgement.
        if self.quit_attempted:
            if not self.quit_ack:
                raise self.base.Refusal("cleanup_incomplete", "original niri Quit acknowledgement unknown")
            return
        self.quit_attempted = True
        with self.owned_connection() as connection:
            connection.sendall(b'{"Action":{"Quit":{"skip_confirmation":true}}}\n')
            deadline = time.monotonic() + 5
            data = b""
            while b"\n" not in data and len(data) <= 4096:
                remaining = deadline - time.monotonic()
                if remaining <= 0: break
                connection.settimeout(remaining)
                block = connection.recv(min(4097 - len(data), 1024))
                if not block: break
                data += block
            if len(data) > 4096 or not data.endswith(b"\n") or data.count(b"\n") != 1:
                raise self.base.Refusal("cleanup_incomplete", "original niri Quit reply incomplete")
            try: reply = json.loads(data)
            except (ValueError, UnicodeError):
                raise self.base.Refusal("cleanup_incomplete", "original niri Quit reply malformed") from None
            if reply != {"Ok": "Handled"}:
                raise self.base.Refusal("cleanup_incomplete", "original niri Quit was not acknowledged")
            self.quit_ack = True

    def stop(self):
        # Failed/timed-out original commands are first settled naturally. No
        # successor, new command, removal or display signal while they are live.
        for record in self.records:
            if record not in self.children.values() and not record["waited"]:
                self.wait(record, 5)
        if self.quit_attempted and not self.quit_ack:
            raise self.base.Refusal("cleanup_incomplete", "original niri Quit acknowledgement unknown")
        niri = self.children.get("niri")
        if niri is not None and not niri["waited"]:
            if niri["child"].poll() is None:
                if self.ipc is None:
                    raise self.base.Refusal("cleanup_incomplete", "original niri started without a usable Quit endpoint")
                self.quit_original()
            self.wait(niri, 10)
        for role in ("kwin", "bus"):
            record = self.children.get(role)
            if record is not None and not record["waited"]:
                if record["child"].poll() is None and role not in self.termination_attempted:
                    self.termination_attempted.add(role)
                    record["child"].terminate()  # Only this retained original display/bus Child, after niri.
                self.wait(record, 10)
        if self.ipc_fd is not None:
            fd, self.ipc_fd = self.ipc_fd, None
            try: os.close(fd)
            except BaseException:
                self.ipc_close_unknown = True
                raise
        if self.ipc_close_unknown:
            raise self.base.Refusal("cleanup_incomplete", "original IPC close unknown")


def install(base):
    class PrebuiltLayout(base.Layout):
        @property
        def target(self):
            return None  # No Cargo/cache target is mounted or selected.

    class PrebuiltFixtures(base.Fixtures):
        def __init__(self, host, package):
            super().__init__(host, {})
            self.package = package
            self.start_lock = base.threading.Lock()
            # Inherited early stop/shutdown may arrive outside fixture.lock.
            # All shared-channel calls AND shutdown receipt state use one lock.
            self.channel_lock = base.threading.RLock()

        def handle(self, request):
            if request["op"] == "start-prebuilt":
                return self.start(request)
            if request["op"] == "stop" and request.get("keep") is not True:
                raise base.Refusal("keep_required", "fixed qualification retains all original evidence; use stop --keep")
            if request["op"] in ("start", "clear", "run-ui", "cli"):
                raise base.Refusal("fixed_recipe_only", "fixed prebuilt mode has no build, arbitrary CLI or stale adoption")
            return super().handle(request)

        def layout_for(self, name, _checkout):
            self.package.verify()
            return PrebuiltLayout(self.host, name, self.host.repo, Path("/"), prebuilt=self.package)

        def start(self, request):
            with self.start_lock:
                with self.lock:
                    if self.live:
                        raise base.Refusal("too_many_fixtures", "fixed prebuilt mode owns one serial private session")
                result = super().start({**request, "checkout": self.host.repo})
                result.update(checkout=None, target=None, source=SOURCE, recipe="a48-recovery")
                return result

        def _create(self, fixture, width):
            layout = fixture.layout
            self._create_owned_root(fixture, self.host.runtime_dir, layout.private_parts,
                                    "private_root_owned", ("niri", "state"))
            self._create_owned_root(fixture, self.host.runtime_dir, (layout.runtime_name,), "runtime_root_owned")
            self._create_owned_root(fixture, self.host.repo, layout.root_parts, "repository_root_owned")
            for name in ("shots", "sandbox/home", "sandbox/data", "sandbox/config", "sandbox/cache",
                         "sandbox/state", "sandbox/bin", "sandbox/project", "sandbox/evidence"):
                fd = base.hold_directory(self.host.repo, (*layout.root_parts, *name.split("/")), create=True)
                fixture.spawn_resources["creation_directory"] = fd
                errors = self._close_spawn_resources(fixture, ("creation_directory",))
                if errors: raise base.Refusal("cleanup_incomplete", "; ".join(errors))
            fixture.desktop = Desktop(base, layout)
            display = fixture.desktop.start()
            fixture.nested_socket = fixture.desktop.ipc
            self._spawn_holder(fixture, display)
            return self._apply_viewport(fixture, width)

        def hold_fixture_binds(self, fixture, display):
            self.package.verify()
            layout, held = fixture.layout, []
            try:
                held.append(os.dup(self.package.fd))
                # Fixed existing offline provider, never a host account/provider.
                provider = self.host.repo / "tests/fixtures/chat-composer-claude.sh"
                # Check special-file metadata without waiting for a FIFO writer.
                held.append(os.open(provider, os.O_RDONLY | os.O_NONBLOCK | os.O_NOFOLLOW | os.O_CLOEXEC))
                before = os.fstat(held[-1])
                if (not stat.S_ISREG(before.st_mode) or before.st_uid != os.geteuid()
                        or stat.S_IMODE(before.st_mode) != 0o755 or before.st_size != 12225):
                    raise base.Refusal("provider_changed", "fixed offline provider input refused")
                content = os.read(held[-1], 128 * 1024 + 1)
                after = os.fstat(held[-1])
                if (hashlib.sha256(content).hexdigest() != PROVIDER_SHA256
                        or (before.st_size, before.st_mtime_ns, before.st_ctime_ns) !=
                           (after.st_size, after.st_mtime_ns, after.st_ctime_ns)):
                    raise base.Refusal("provider_changed", "fixed offline provider digest or identity changed")
                held.append(base.hold_directory(self.host.repo, (*layout.root_parts, "sandbox")))
                held.append(base.hold_directory(self.host.runtime_dir, (layout.runtime_name,)))
                held.append(base.hold_socket(layout.niri_root, display))
                return base.Binds(*held)
            except BaseException as original:
                errors = []
                for fd in held:
                    try: os.close(fd)
                    except BaseException as error: errors.append(f"partial prebuilt bind close: {error}")
                if errors: original.broker_close_errors = errors
                raise

        def holder_args(self, fixture, binds, info, relay):
            layout = fixture.layout
            args = ["--unshare-all", "--as-pid-1", "--cap-drop", "ALL", "--ro-bind", "/usr", "/usr", "--ro-bind", "/etc", "/etc"]
            for name in ("bin", "sbin", "lib", "lib64"):
                path = Path("/") / name
                if path.is_symlink(): args += ["--symlink", os.readlink(path), str(path)]
                elif path.is_dir(): args += ["--ro-bind", str(path), str(path)]
            args += ["--proc", "/proc", "--dev", "/dev", "--dev-bind-try", "/dev/dri", "/dev/dri",
                     "--ro-bind-try", "/sys", "/sys", "--tmpfs", "/tmp", "--tmpfs", "/var",
                     "--ro-bind-fd", str(binds.checkout), MOUNT,
                     "--bind-fd", str(binds.sandbox), "/fixture",
                     "--ro-bind-fd", str(binds.target), "/fixture/bin/claude",
                     "--bind-fd", str(binds.runtime), "/run/cagents-fixture",
                     "--bind-fd", str(binds.wayland), "/run/cagents-fixture/nested-wayland",
                     "--chdir", "/fixture/project", "--remount-ro", "/"]
            return ["/usr/bin/prlimit", *base.SANDBOX_RLIMITS, "/usr/bin/bwrap", "--info-fd", str(info),
                    *args, "--", "/usr/bin/python3", "-I", "-c", RELAY, str(relay)]

        def prebuilt_step(self, fixture, request):
            with self.channel_lock:
                return self._prebuilt_step(fixture, request)

        def _prebuilt_step(self, fixture, request):
            self.package.verify()
            reply = fixture.channel.call({"action": request["step"]}, 40)
            self.package.verify()
            if "refused" in reply:
                raise base.Refusal("prebuilt_refused", reply["refused"])
            if reply.get("failed") is True or reply.get("settled") is False or reply.get("exit_code", 0) != 0:
                raise base.Refusal("prebuilt_negative", "original private step failed or wait remains unsettled; no next phase")
            return {key: value for key, value in reply.items() if key != "id"}

        def status(self, fixture, _request):
            with self.channel_lock:
                return self._prebuilt_status(fixture)

        def _prebuilt_status(self, fixture):
            return {"fixture": fixture.layout.name, "source": SOURCE, "building": False,
                    "private": True, "runtime": str(fixture.layout.runtime),
                    "children": fixture.channel.call({"action": "status"}, 10).get("children"),
                    "native_recovery_qualified": False}

        def _is_building(self, fixture):
            return False

        def _nested_env(self, fixture):
            return fixture.desktop.env

        def _niri_json(self, env, query):
            for fixture in self.live.values():
                if hasattr(fixture, "desktop") and env is fixture.desktop.env:
                    result = fixture.desktop.command([self.host.niri, "msg", "--json", query])
                    return json.loads(result.stdout) if result.returncode == 0 else None
            raise base.Refusal("headless_failed", "no original private display owns this query")

        def _apply_viewport(self, fixture, width):
            fixture.width = width
            return self._viewport_report(fixture, width)

        def viewport(self, fixture, request):
            if request["width"] != fixture.width:
                raise base.Refusal("fixed_viewport", "prebuilt recipe keeps the original viewport")
            return self._viewport_report(fixture, fixture.width)

        def _drive(self, fixture, command):
            result = fixture.desktop.command(base.harness_argv(self.host, fixture.layout.name, command), timeout=base.HARNESS_SECONDS)
            return {"exit_code": result.returncode, "stdout": base.bounded(result.stdout), "stderr": base.bounded(result.stderr)}

        def _kill_holder(self, fixture):
            with self.channel_lock:
                return self._settle_prebuilt_holder(fixture)

        def _settle_prebuilt_holder(self, fixture):
            # Override ALL inherited stop/shutdown call sites: never kill this
            # namespace or an unproven retained provider to make cleanup pass.
            errors = []
            if fixture.process is None:
                settled = True
            else:
                try:
                    if not getattr(fixture, "prebuilt_shutdown_ack", False):
                        reply = fixture.channel.call({"action": "shutdown"}, 10)
                        if reply.get("settled") is not True:
                            raise base.Refusal("cleanup_incomplete", reply.get("refused", "original namespace not settled"))
                        fixture.prebuilt_shutdown_ack = True
                        fixture.prebuilt_adopted_waits = reply.get("adopted_waits", [])
                    if not getattr(fixture, "prebuilt_receipt_complete", False):
                        if getattr(fixture, "prebuilt_receipt_attempted", False):
                            raise base.Refusal("cleanup_incomplete", "original private receipt writer remains incomplete")
                        fixture.prebuilt_receipt_attempted = True
                        # Private physical-exit evidence, never provider completion
                        # or replay/alias/approval authority. No raw record returned
                        # through the Agent request schema.
                        payload = (json.dumps({"adopted_waits": fixture.prebuilt_adopted_waits}, sort_keys=True) + "\n").encode()
                        fd = os.open(fixture.layout.private / "adopted-waits.json",
                                     os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600)
                        fixture.spawn_resources["adopted_receipt_fd"] = fd
                        offset = 0
                        while offset < len(payload):
                            written = os.write(fd, payload[offset:])
                            if written <= 0: raise OSError("private receipt writer made no progress")
                            offset += written
                        errors = self._close_spawn_resources(fixture, ("adopted_receipt_fd",))
                        if errors: raise base.Refusal("cleanup_incomplete", "; ".join(errors))
                        fixture.prebuilt_receipt_complete = True
                    fixture.process.wait(timeout=10)
                    settled = bool(select.select([fixture.pidfd], [], [], 0)[0]) if fixture.pidfd is not None else False
                except BaseException as error:
                    errors.append(f"original prebuilt cleanup: {type(error).__name__}: {error}")
                    settled = False
            fixture.settlement = {"settled": settled, "original_wait": settled,
                                  "held_pidfd_exited": settled, "errors": errors}
            return fixture.settlement

        def stop(self, fixture, request):
            result = super().stop(fixture, {"keep": True})
            result["retained_paths"] = [str(fixture.layout.root), str(fixture.layout.private), str(fixture.layout.runtime)]
            return result

        def _discard(self, layout, *, remove):
            fixture = self.live.get(layout.name)
            if fixture is None:
                raise base.Refusal("cleanup_incomplete", "no original prebuilt owner; stale root is retained")
            if hasattr(fixture, "desktop"):
                fixture.desktop.stop()
            fixture.host_session_stopped = True
            self._release(fixture)
            # All three exclusively created roots are intentionally retained as
            # original evidence, including display logs and adopted wait receipts.
            # Neither ordinary discard nor stale recovery deletes/adopts them.
            return []

        def recover_stale(self):
            return [], []  # No re-adoption, process reconstruction or stale deletion.

    return PrebuiltFixtures


def main(base):
    # Keep this boundary even when the support module is called directly.
    refused = base.guard()
    if refused is not None:
        return refused
    missing = [tool for tool in ("/usr/bin/kwin_wayland", "/usr/bin/dbus-daemon", "/usr/bin/bwrap",
                                "/usr/bin/prlimit", "/usr/bin/python3") if not os.access(tool, os.X_OK)]
    import shutil
    missing += [tool for tool in ("niri", "niri-harness") if shutil.which(tool) is None]
    if missing:
        print("fixed prebuilt carrier missing tools: " + ", ".join(missing), file=sys.stderr)
        return 2
    host = base.host_from_environment()
    # Fixed mode owns roots/source beneath the actual carrier checkout, not
    # repository_root assumptions about linked-worktree Git metadata layout.
    from dataclasses import replace
    host = replace(host, repo=Path(base.__file__).resolve().parents[1])
    metadata = host.runtime_dir.lstat()
    if not stat.S_ISDIR(metadata.st_mode) or metadata.st_uid != os.geteuid() or stat.S_IMODE(metadata.st_mode) != 0o700:
        print("fixed prebuilt carrier requires the original owned mode0700 runtime", file=sys.stderr)
        return 2
    package = Package(base)
    fixtures = install(base)(host, package)
    path = host.runtime_dir / base.SOCKET_NAME
    server = base.Server(path, host.repo, fixtures.handle)
    try:
        base.install_termination()
        print("fixed a48 prebuilt broker; native/recovery qualification unrun", flush=True)
        server.serve_forever()
    except KeyboardInterrupt:
        pass
    finally:
        # Existing shutdown keeps original requests/spawner/live owners until
        # their positive settlement. No new resource-release or kill fallback.
        failed = base.shutdown(server, fixtures)
        path.unlink(missing_ok=True)
        package.close()
    return 1 if failed else 0
