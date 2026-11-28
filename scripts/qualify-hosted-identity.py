#!/usr/bin/python3
"""Exercise inherited process identity with private Unix socket peers."""
import argparse
from contextlib import ExitStack, closing, contextmanager
import ctypes
import json
import os
from pathlib import Path
import plistlib
import select
import signal
import socket
import struct
import subprocess
import sys
import tempfile
import time
import uuid

MACOS = sys.platform == "darwin"
TIMEOUT = 15
SO_PEERPIDFD = 77
SOL_LOCAL = 0
LOCAL_PEERPID = 2
PROC_PIDCOALITIONINFO = 20


def require(condition, message):
    if not condition:
        raise RuntimeError(message)


def wait_for(predicate, description):
    deadline = time.monotonic() + TIMEOUT
    while not predicate():
        require(time.monotonic() < deadline, f"timed out waiting for {description}")
        time.sleep(0.02)


def fixture(*args):
    return [sys.executable, "-I", "-B", str(Path(__file__).resolve()), *map(str, args)]


def run(args, **kwargs):
    return subprocess.run(args, check=True, timeout=TIMEOUT, **kwargs)


def proc_values(path, field):
    prefix = field + ":"
    return next(line.removeprefix(prefix).split() for line in path.read_text().splitlines()
                if line.startswith(prefix))


@contextmanager
def linux_peer(peer):
    require(os.readlink("/proc/self") == str(os.getpid()),
            "procfs is not in the caller's PID namespace")
    pid, uid, _ = struct.unpack("iII", peer.getsockopt(socket.SOL_SOCKET, socket.SO_PEERCRED, 12))
    # SO_PEERPIDFD names the connecting task even if its numeric PID is reused.
    fd, = struct.unpack("i", peer.getsockopt(socket.SOL_SOCKET, SO_PEERPIDFD, 4))
    try:
        watch = select.poll()
        watch.register(fd, select.POLLIN)
        require(pid > 0 and not watch.poll(0), "Linux socket peer is not visible and live")
        yield pid, uid, fd
        require(not watch.poll(0), "Linux socket peer exited during the query")
    finally:
        os.close(fd)


def linux_identity(peer):
    with linux_peer(peer) as (pid, uid, _):
        process = Path("/proc") / str(pid)
        cgroup = next(line[3:] for line in (process / "cgroup").read_text().splitlines()
                      if line.startswith("0::"))
        parent, = proc_values(process / "status", "PPid")
        return pid, uid, int(parent), cgroup


def linux_scope_environment():
    runtime = Path(f"/run/user/{os.geteuid()}")
    path = runtime / "systemd/private"
    require(path.resolve() == path, "systemd manager socket uses a symlink")
    # systemd-run can fall back to sending numeric PIDs to the manager. Require
    # the same namespace before allowing any scope launch.
    with socket.socket(socket.AF_UNIX) as manager:
        manager.settimeout(TIMEOUT)
        manager.connect(str(path))
        with linux_peer(manager) as (pid, uid, fd):
            require(uid == os.geteuid()
                    and proc_values(Path("/proc/self/status"), "NSpid") == [str(os.getpid())]
                    and proc_values(Path(f"/proc/self/fdinfo/{fd}"), "NSpid") == [str(pid)],
                    "user manager PID namespace not verified")
    env = os.environ.copy()
    env["XDG_RUNTIME_DIR"] = str(runtime)
    env.pop("DBUS_SESSION_BUS_ADDRESS", None)
    return env


def macos_identity(peer):
    lib = ctypes.CDLL("/usr/lib/libproc.dylib", use_errno=True)
    # LOCAL_PEERPID is a numeric candidate. Keep its lifetime pinned through
    # the query, as the daemon does, and confirm the socket still names it.
    pid = peer.getsockopt(SOL_LOCAL, LOCAL_PEERPID)
    uid, gid = ctypes.c_uint(), ctypes.c_uint()
    if lib.getpeereid(peer.fileno(), ctypes.byref(uid), ctypes.byref(gid)) != 0:
        raise OSError(ctypes.get_errno(), "getpeereid")
    query = lib.proc_pidinfo
    query.argtypes = (ctypes.c_int, ctypes.c_int, ctypes.c_uint64, ctypes.c_void_p, ctypes.c_int)
    # XNU's proc_pidcoalitioninfo has resource, jetsam and three reserved IDs.
    coalitions = (ctypes.c_uint64 * 5)()
    with closing(select.kqueue()) as watch:
        watch.control([select.kevent(pid, filter=select.KQ_FILTER_PROC,
                                     flags=select.KQ_EV_ADD, fflags=select.KQ_NOTE_EXIT)], 0, 0)
        size = ctypes.sizeof(coalitions)
        written = query(pid, PROC_PIDCOALITIONINFO, 0, coalitions, size)
        require(written == size and coalitions[0] != 0,
                f"resource coalition query failed (bytes={written}, errno={ctypes.get_errno()})")
        parent = int(run(["/bin/ps", "-o", "ppid=", "-p", str(pid)], capture_output=True).stdout)
        require(not watch.control(None, 1, 0) and peer.getsockopt(SOL_LOCAL, LOCAL_PEERPID) == pid,
                "macOS socket peer changed during the query")
    return pid, uid.value, parent, coalitions[0]


def observe(root, phase):
    names = ({"owner", "child", "external"} if phase == "first"
             else {"owner", "retained", "detached", "external"})
    path = root / "peer.sock"
    report = {}
    with socket.socket(socket.AF_UNIX) as listener:
        listener.settimeout(TIMEOUT)
        listener.bind(str(path))
        listener.listen()
        (root / (phase + "-ready")).touch()
        for _ in names:
            peer, _ = listener.accept()
            with peer:
                peer.settimeout(TIMEOUT)
                with peer.makefile("rb") as stream:
                    name = stream.readline(32).decode().rstrip("\n")
                require(name in names and name not in report, f"unexpected peer: {name!r}")
                pid, uid, parent, identity = (macos_identity if MACOS else linux_identity)(peer)
                require(uid == os.geteuid(), f"unexpected peer uid {uid}")
                report[name] = dict(pid=pid, parent=parent, identity=identity)
                peer.sendall(b"x")
            if name in {"owner", "retained"} and {"owner", "retained"} <= report.keys():
                (root / "release-owner").touch()
    path.unlink()
    print(json.dumps(dict(observer=os.getpid(), peers=report)))


def connect(root, phase, name):
    wait_for((root / (phase + "-ready")).exists, phase + " observer")
    with socket.socket(socket.AF_UNIX) as peer:
        peer.settimeout(TIMEOUT)
        peer.connect(str(root / "peer.sock"))
        peer.sendall((name + "\n").encode())
        require(peer.recv(1) == b"x", f"{phase} observer did not acknowledge {name}")


def scope_command(unit, args):
    return ["/usr/bin/systemd-run", "--user", "--scope", "--quiet", "--collect",
            "--unit", unit, *args]


def owner(root, late_unit):
    connect(root, "first", "owner")
    # Create a new session and exec the payload. Give detached work its own
    # deadline so it exits even if the controller or a socket observer fails.
    subprocess.Popen(fixture("child", root, os.getpid()), start_new_session=True,
                     stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL)
    wait_for((root / "second-ready").exists, "replacement observer")
    if late_unit:
        # The scope launcher enrolls itself and execs in place. The controller
        # never submits another process's numeric PID to the user manager.
        command = scope_command(late_unit, fixture("retained-owner", root))
        os.execve(command[0], command, linux_scope_environment())
    retained_owner(root)


def retained_owner(root):
    connect(root, "second", "owner")
    wait_for((root / "release-owner").exists, "owner release")


def child(root, owner_pid):
    connect(root, "first", "child")
    connect(root, "second", "retained")
    wait_for(lambda: os.getppid() != owner_pid, "reparenting")
    connect(root, "second", "detached")


def stop_process(process):
    if process.poll() is None:
        process.kill()
    process.communicate(timeout=TIMEOUT)


def run_observer(stack, root, phase):
    process = subprocess.Popen(fixture("observe", root, phase), stdout=subprocess.PIPE)
    stack.callback(stop_process, process)
    return process


def read_report(process):
    output, _ = process.communicate(timeout=TIMEOUT)
    require(process.returncode == 0, f"observer exited {process.returncode}")
    return json.loads(output)


def external(root, phase):
    # Start from an ordinary shell independent of the owner and its scope/job.
    run(["/bin/sh", "-c", '"$@"; exit $?', "sh", *fixture("external", root, phase)])


def start_owner(stack, root, mode, unit):
    args = fixture("owner", root, unit if mode == "late" else "")
    if MACOS and mode != "fallback":
        domain = f"{mode}/{os.getuid()}"
        job = root / "owner.plist"
        job.write_bytes(plistlib.dumps({
            "Label": unit, "ProgramArguments": args, "RunAtLoad": True,
            "AbandonProcessGroup": True, "StandardErrorPath": str(root / "owner.err"),
            "LimitLoadToSessionType": "Aqua" if mode == "gui" else "Background",
        }))
        loaded = False

        def bootout(exception_type, _exception, _traceback):
            try:
                run(["/bin/launchctl", "bootout", f"{domain}/{unit}"], capture_output=True)
            except (subprocess.SubprocessError, OSError) as error:
                # A refused bootstrap did not necessarily load anything. Keep
                # the original failure if cleanup cannot resolve the job.
                if loaded and exception_type is None:
                    raise
                if loaded or not isinstance(error, subprocess.CalledProcessError):
                    print(f"launchd job cleanup failed: {error}", file=sys.stderr)

        stack.push(bootout)
        run(["/bin/launchctl", "bootstrap", domain, str(job)])
        loaded = True
        return None
    env = None
    if mode != "fallback":
        env = linux_scope_environment()
        stack.callback(subprocess.run, ["/usr/bin/systemctl", "--user", "stop", unit],
                       timeout=TIMEOUT, env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    if mode == "early":
        args = scope_command(unit, args)
    process = subprocess.Popen(args, stdin=subprocess.DEVNULL, env=env)
    stack.callback(stop_process, process)
    return process


def qualify(root, mode):
    unit = "cagents-session-" + uuid.uuid4().hex + ("" if MACOS else ".scope")
    with ExitStack() as stack:
        initial = run_observer(stack, root, "first")
        original = start_owner(stack, root, mode, unit)
        external(root, "first")
        first = read_report(initial)
        if mode == "early":
            marker = root / "unexpected-exec"
            refused = subprocess.run(scope_command(unit, ["/usr/bin/touch", str(marker)]),
                                     capture_output=True, timeout=TIMEOUT,
                                     env=linux_scope_environment())
            require(refused.returncode != 0 and not marker.exists(),
                    "payload ran when an existing unit made enrollment fail")
        replacement = run_observer(stack, root, "second")
        external(root, "second")
        second = read_report(replacement)
        if original is not None:
            require(original.wait(timeout=TIMEOUT) == 0, "owner exited unsuccessfully")
    hosted, later = first["peers"], second["peers"]
    owner_pid = hosted["owner"]["pid"]
    report = dict(mode=mode, first=first, replacement=second)
    if mode == "early":
        report["required_enrollment_refused_payload"] = True
    print(json.dumps(report, indent=2, sort_keys=True))
    require(first["observer"] != second["observer"], "observer process was not replaced")
    payload = hosted["child"]
    require(payload["parent"] == later["retained"]["parent"] == later["owner"]["pid"] == owner_pid,
            "the owner was absent before the detached case")
    require(later["detached"]["parent"] != owner_pid, "payload did not reparent")
    require(later["retained"]["pid"] == later["detached"]["pid"] == payload["pid"],
            "observations did not follow the same payload")
    require(payload["identity"] == later["retained"]["identity"] == later["detached"]["identity"],
            "payload identity changed after exec or reparenting")
    require(hosted["external"]["identity"] == later["external"]["identity"],
            "external identity changed during the probe")
    if mode in ("late", "fallback"):
        require(payload["identity"] == hosted["owner"]["identity"] == hosted["external"]["identity"],
                "unenrolled owner or payload unexpectedly had an isolated identity")
    else:
        require(payload["identity"] == hosted["owner"]["identity"] != hosted["external"]["identity"],
                "payload did not inherit the owner's separate launch identity")
    if mode in ("early", "late"):
        require(later["owner"]["identity"].endswith("/" + unit), "owner missed its assigned unit")
    else:
        require(later["owner"]["identity"] == hosted["owner"]["identity"],
                "owner identity changed before release")
    if mode == "early":
        require(payload["identity"].endswith("/" + unit), "payload missed its assigned unit")
    if not MACOS:
        require("/cagents-session-" not in hosted["external"]["identity"],
                "external peer belongs to a Cagents session scope")


def interrupted(signum, _frame):
    raise SystemExit(128 + signum)


def main():
    if len(sys.argv) > 1:
        role = sys.argv[1]
        if role == "observe":
            return observe(Path(sys.argv[2]), sys.argv[3])
        if role == "owner":
            return owner(Path(sys.argv[2]), sys.argv[3])
        if role == "retained-owner":
            return retained_owner(Path(sys.argv[2]))
        if role == "child":
            return child(Path(sys.argv[2]), int(sys.argv[3]))
        if role == "external":
            return connect(Path(sys.argv[2]), sys.argv[3], "external")
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--domain", choices=("gui", "user"), default="gui",
                        help="macOS launchd domain to qualify (default: gui)")
    args = parser.parse_args()
    if sys.platform not in ("darwin", "linux") or os.geteuid() == 0:
        parser.error("run from a Linux or macOS non-root user session")
    if "CAGENTS_AGENT_AUTHORITY" in os.environ:
        parser.error("use an external user session so the control peer is outside Cagents custody")
    for signum in (signal.SIGTERM, signal.SIGHUP):
        signal.signal(signum, interrupted)
    for mode in (args.domain, "fallback") if MACOS else ("early", "late", "fallback"):
        with tempfile.TemporaryDirectory(prefix="cagents-identity-", dir="/var/tmp") as directory:
            qualify(Path(directory), mode)


if __name__ == "__main__":
    main()
