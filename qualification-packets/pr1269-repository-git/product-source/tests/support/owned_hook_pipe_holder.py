#!/usr/bin/python3
"""Private Git-hook pipe fixture. It never discovers or signals a process."""
import array
import fcntl
import json
import os
import select
import socket
import stat
import sys
import time

ADDRESS = __ADDRESS_JSON__
COOKIE = __COOKIE_JSON__
LIMIT = 4096


def emit(value):
    print(json.dumps(dict(value, cookie=COOKIE), sort_keys=True), flush=True)


def identity(fd):
    info = os.fstat(fd)
    assert stat.S_ISFIFO(info.st_mode), "capture descriptor is not a pipe"
    assert fcntl.fcntl(fd, fcntl.F_GETFL) & os.O_ACCMODE != os.O_RDONLY
    return [info.st_dev, info.st_ino]


def hook():
    payload = json.dumps({"cookie": COOKIE, "writers": [identity(1), identity(2)],
                          "hook_group": os.getpgrp()}).encode() + b"\n"
    assert len(payload) <= LIMIT
    with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as client:
        client.settimeout(5)
        client.connect(ADDRESS)
        sent = client.sendmsg([payload], [(socket.SOL_SOCKET, socket.SCM_RIGHTS,
                                          array.array("i", [1, 2]))])
        assert sent > 0
        client.sendall(payload[sent:])
        client.shutdown(socket.SHUT_WR)
        acknowledgement = b""
        while len(acknowledgement) < 3:
            part = client.recv(3 - len(acknowledgement))
            assert part, "holder closed before acknowledging actual writers"
            acknowledgement += part
        assert acknowledgement == b"OK\n"
        assert client.recv(1) == b"", "trailing acknowledgement data"


def holder():
    writers = []
    control = bytearray()

    def command():
        chunk = os.read(0, LIMIT + 1)
        if not chunk:
            return "eof"
        control.extend(chunk)
        assert len(control) <= LIMIT, "oversized control command"
        if b"\n" not in control:
            return None
        line, _, rest = control.partition(b"\n")
        control[:] = rest
        assert not rest, "interleaved control commands"
        assert line == ("snapshot " + COOKIE).encode(), "unexpected control command"
        return "snapshot"

    def ready_for(fd, deadline=None):
        while True:
            timeout = None if deadline is None else max(0, deadline - time.monotonic())
            assert timeout is None or timeout > 0, "descriptor handoff timed out"
            readable, _, _ = select.select([0, fd], [], [], timeout)
            assert readable, "descriptor handoff timed out"
            if 0 in readable:
                action = command()
                if action == "eof":
                    return False
                assert action is None, "snapshot before writer handoff"
            if fd in readable:
                return True

    try:
        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as server:
            server.bind(ADDRESS)
            server.listen(1)
            emit({"state": "ready", "holder_group": os.getpgrp()})
            if not ready_for(server):
                return
            connection, _ = server.accept()
            with connection:
                deadline = time.monotonic() + 5
                payload = b""
                first = True
                while True:
                    if not ready_for(connection, deadline):
                        return
                    chunk, ancillary, flags, _ = connection.recvmsg(
                        LIMIT + 1 - len(payload),
                        socket.CMSG_SPACE(2 * array.array("i").itemsize),
                        getattr(socket, "MSG_CMSG_CLOEXEC", 0))
                    # Own all delivered rights before rejecting any frame,
                    # including unexpected rights on continuation chunks.
                    unexpected = False
                    for level, kind, data in ancillary:
                        if level != socket.SOL_SOCKET or kind != socket.SCM_RIGHTS:
                            unexpected = True
                            continue
                        values = array.array("i")
                        complete = len(data) - len(data) % values.itemsize
                        values.frombytes(data[:complete])
                        writers.extend(values)
                        unexpected |= complete != len(data)
                    assert not flags & (socket.MSG_CTRUNC | socket.MSG_TRUNC)
                    assert not unexpected
                    if first:
                        assert len(ancillary) == 1 and len(writers) == 2
                        first = False
                    else:
                        assert not ancillary, "rights on a continuation frame"
                    if not chunk:
                        break
                    payload += chunk
                    assert len(payload) <= LIMIT, "oversized handoff"
                # Decode only after the hook's write-half EOF. A delayed suffix
                # or second frame can never arrive after this acceptance point.
                assert len(payload) <= LIMIT and payload.endswith(b"\n")
                assert payload.count(b"\n") == 1
                def unique_object(pairs):
                    value = {}
                    for key, item in pairs:
                        assert key not in value, "duplicate handoff key"
                        value[key] = item
                    return value

                request = json.loads(payload, object_pairs_hook=unique_object)
                assert type(request) is dict and set(request) == {
                    "cookie", "writers", "hook_group"}
                assert request["cookie"] == COOKIE
                assert type(request["hook_group"]) is int
                assert request["hook_group"] != os.getpgrp()
                actual = [identity(fd) for fd in writers]
                assert actual == request["writers"]
                assert actual[0] != actual[1], "capture writers must be distinct pipes"
                connection.settimeout(5)
                connection.sendall(b"OK\n")
        # Neither a timer nor Git's group teardown releases these real writers.
        # Only the original parent's control EOF does so.
        state = {"state": "holding", "writers": actual,
                 "holder_group": os.getpgrp(), "hook_group": request["hook_group"]}
        emit(state)
        while True:
            select.select([0], [], [])
            action = command()
            if action == "eof":
                return
            if action == "snapshot":
                assert [identity(fd) for fd in writers] == actual
                emit(state)
    finally:
        for fd in writers:
            os.close(fd)


if sys.argv[1:] == ["--holder"]:
    holder()
else:
    assert not sys.argv[1:]
    hook()
