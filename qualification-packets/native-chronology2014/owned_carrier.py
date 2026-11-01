#!/usr/bin/env python3
"""Future fixed After13ed carrier; never use to repair original session 22066.

Run only under a separately reviewed copy of the existing canonical-C flock /
private dbus-run-session / KWin --exit-with-session route. That caller still
owns source/context/resource admission and the outer wait. Product source is
the frozen SOURCE below; standard run-development, jobs1 and the warm target
are unchanged. Only the designated native input owner drives the saved session.
The future caller must set NIRI_HARNESS_STATE to CASE/harness-state; the frozen
future request records the exact executable carrier and private context.

This file owns Niri directly instead of the short-lived harness session-start
CLI. EOF from normal Quit is missing ACK, not process death: only the retained
original Child.wait records reap. No Session.alive, PID adoption, kill, group
signal or timeout termination exists here. HOLD keeps the original handles and
continues observing that same wait. A private cancel-request is cooperative and
negative; it cannot force an unresponsive compositor or an unknown wait to end.
The platform's missing original-execution-controller stop/wait remains a limit.
"""

import hashlib
import json
import os
from pathlib import Path
import runpy
import socket
import stat
import struct
import subprocess
import time


SOURCE_HEAD = "f71ce673f25f5a64f3ac393349c4e64099b7145f"
SOURCE = Path("/home/marvin/Documents/repositories/cagents/.worktrees/fix-2014-capacity-chronology-before")
CASE = Path("/tmp/issue2014-native-evidence/future-owned-before-f71ce-wide")
TARGET = Path("/home/marvin/Documents/repositories/cagents/.worktrees/validation-three-lanes/lane-c-target")
HARNESS = Path("/home/marvin/.local/lib/niri-harness/bin/niri-harness")
HARNESS_SHA256 = "b9c278ed2e496f68a12e5bafdc0f04d9deb299ca6b8294dbc95630d61ce2e7d2"
CONFIG_SHA256 = "dac9fc580743713f13bc43718c16d61841d0596ddc149050eefb6871fd7d7173"
SESSION = "issue2014-future-owned-before-f71ce-wide"
QUIT = {"Action": {"Quit": {"skip_confirmation": True}}}


def one_output(response):
    """Validate the actual Outputs response variant before releasing the runner."""
    if not isinstance(response, dict) or set(response) != {"Outputs"}:
        raise RuntimeError("expected the private Outputs response variant")
    outputs = response["Outputs"]
    if not isinstance(outputs, dict) or len(outputs) != 1:
        raise RuntimeError("expected exactly one owned nested output")
    return outputs


def record_observation(state, output):
    """Persist before publishing; neither observation is a terminal return."""
    observation = dict(state)
    observation["proposed_exit_code"] = observation.pop("exit_code")
    observation["terminal_return_confirmed"] = False
    if not state["pre_spawn_refused"]:
        output.persist_state(observation)
    output.publish_state(observation)


class MissingAck(Exception):
    pass


class Peer:
    """One retained connection, bound to the unreaped original Child PID."""

    def __init__(self, stream, child_pid, uid):
        self.stream, self.child_pid, self.uid = stream, child_pid, uid
        self.check()

    def check(self):
        pid, uid, _ = struct.unpack(
            "3i", self.stream.getsockopt(socket.SOL_SOCKET, socket.SO_PEERCRED, 12)
        )
        if (pid, uid) != (self.child_pid, self.uid):
            raise RuntimeError("private peer is not the retained original Niri Child")

    def request(self, request):
        self.check()
        self.stream.sendall(json.dumps(request, separators=(",", ":")).encode() + b"\n")
        data = bytearray()
        while not data.endswith(b"\n"):
            chunk = self.stream.recv(1)
            if not chunk:
                raise MissingAck("EOF before reply: " + bytes(data).decode(errors="replace"))
            data.extend(chunk)
            if len(data) > 65536:
                raise RuntimeError("oversized private Niri reply")
        reply = json.loads(data)
        if not isinstance(reply, dict) or set(reply) != {"Ok"}:
            raise RuntimeError("private Niri refused request: " + repr(reply))
        return reply["Ok"]


class Carrier:
    """Fixed chronology lifecycle. Ports are replaced in offline contracts."""

    def __init__(self, ports):
        self.ports = ports
        self.child = self.pin = self.peer = None
        self.start_attempted = False
        self.state = dict(
            phase="not-started", source_head=SOURCE_HEAD, peer_bound=False,
            runner_released=False, runner_status=None, native_status=None,
            quit_attempted=False, quit_ack="not-attempted", child_wait_complete=False,
            child_status=None, cancelled=False, blocked=False, errors=[],
            cleanup_complete=False, owned_close_complete=False,
            exit_code=None, pre_spawn_refused=False,
        )

    def hold(self, where, error):
        self.state.update(phase="HOLD", blocked=True, cleanup_complete=False, exit_code=None)
        item = where + ": " + type(error).__name__ + ": " + str(error)
        if item not in self.state["errors"]:
            self.state["errors"].append(item)

    def start(self):
        self.start_attempted = True
        try:
            self.ports.prepare()
        except BaseException as error:
            # prepare cannot launch a child or open the retained output owners.
            # Record this known refusal on the original outer stream even when
            # preparation never created a writable private evidence directory.
            self.state.update(
                phase="pre-spawn-refused", pre_spawn_refused=True,
                cleanup_complete=True, exit_code=2,
            )
            self.state["errors"].append("prepare: " + type(error).__name__ + ": " + str(error))
            return
        try:
            # Assignment immediately retains the returned original object.
            self.child = self.ports.spawn()
            self.state.update(phase="original-child-retained", original_pid=self.child.pid)
            self.pin = self.ports.pin(self.child)
            self.state["pidfd_retained"] = True
        except BaseException as error:
            self.hold("startup", error)

    def step(self):
        """One observation; never replace, signal or abandon an original."""
        try:
            self.state["cancelled"] |= self.ports.cancel_requested()
        except BaseException as error:
            self.hold("cancel-observation", error)
        if self.child is None:
            return
        if not self.state["child_wait_complete"]:
            try:
                status = self.ports.wait_original(self.child)
                if status is not None:
                    self.state.update(child_wait_complete=True, child_status=status)
            except BaseException as error:
                self.hold("original-wait", error)
        if self.state["blocked"]:
            return  # Still observe the exact original on the next step.
        try:
            if not self.state["runner_released"]:
                if self.state["child_wait_complete"]:
                    raise RuntimeError("original Niri exited before registered runner release")
                registration = self.ports.registration()
                if registration is None:
                    self.state["phase"] = "waiting-private-handshake"
                    return
                self.peer = self.ports.bind_peer(self.child, self.pin, registration)
                self.state["peer_bound"] = True
                self.ports.register(self.child, self.peer, registration)
                self.ports.release_runner()
                self.state.update(runner_released=True, phase="runner-released")
                self.state["native_status"] = self.ports.observe_native()
            status = self.ports.runner_status()
            if status is not None:
                self.state["runner_status"] = status
            if self.state["runner_status"] is None:
                if self.state["child_wait_complete"]:
                    raise RuntimeError("Niri exited with runner completion still unknown")
                self.state["phase"] = "waiting-runner"
                return
            if not self.state["quit_attempted"]:
                # Record attempt before the write: never replay an ambiguous Quit.
                self.state.update(quit_attempted=True, phase="normal-quit")
                if self.state["child_wait_complete"]:
                    raise RuntimeError("original exited before its normal Quit request")
                try:
                    if self.ports.normal_quit(self.peer) != "Handled":
                        raise RuntimeError("unexpected normal Quit acknowledgement")
                    self.state["quit_ack"] = "acknowledged"
                except MissingAck as error:
                    self.state.update(quit_ack="missing-eof", quit_diagnostic=str(error))
                except BaseException as error:
                    self.state.update(quit_ack="error", quit_diagnostic=str(error))
                # Both error cases still reach the original wait on the next step.
            self.state["phase"] = "waiting-original-niri"
            if self.state["child_wait_complete"]:
                self.ports.close_owned(self.peer, self.pin)
                self.state["owned_close_complete"] = True  # Irreversible fact.
                negative = (self.state["cancelled"]
                            or self.state["quit_ack"] != "acknowledged"
                            or self.state["child_status"] != 0)
                self.state.update(
                    cleanup_complete=True, phase="owned-cleanup-settled",
                    exit_code=(2 if negative else self.state["runner_status"] or self.state["native_status"]),
                )
        except BaseException as error:
            self.hold("lifecycle", error)

    def turn(self):
        """The actual bounded loop body is also the offline failure seam."""
        if not self.state["cleanup_complete"]:
            self.step()
        try:
            self.ports.record(self.state)
        except BaseException as error:
            if self.state["pre_spawn_refused"]:
                # No process/output owner was created. A failed diagnostic
                # cannot turn this proven no-spawn refusal into retained custody.
                self.state["errors"].append("evidence: " + type(error).__name__ + ": " + str(error))
            else:
                self.hold("evidence", error)
        if not self.state["cleanup_complete"]:
            try:
                self.ports.pause()
            except BaseException as error:
                self.hold("owner-interrupted", error)

    def owner_turn(self):
        """Retain the owner for catchable interruption outside adapter catches."""
        try:
            self.turn()
        except BaseException as error:
            self.hold("owner-loop", error)

    def run(self):
        while True:
            try:
                if not self.start_attempted and not self.state["blocked"]:
                    self.start_attempted = True
                    self.start()
                self.owner_turn()
                if self.state["cleanup_complete"]:
                    return self.state["exit_code"]
            except BaseException as error:
                # Keep this same owner even at startup/loop/return boundaries.
                # Uncatchable termination and the absent platform controller
                # remain outside this cooperative process's guarantees.
                self.hold("owner-loop", error)


class LivePorts:
    """Real adapter for a FUTURE admitted run; offline tests never construct it."""

    def __init__(self):
        self.outputs = []
        self.pending_stream = None
        self.registration_value = None

    @staticmethod
    def write_new(path, text):
        with path.open("x", encoding="utf-8") as stream:
            stream.write(text)

    def prepare(self):
        # The existing outer route prepares this fresh private case before KWin.
        info = CASE.lstat()
        if not stat.S_ISDIR(info.st_mode) or info.st_uid != os.getuid() or info.st_mode & 0o077:
            raise RuntimeError("expected the exclusively owned private future case")
        if hashlib.sha256(HARNESS.read_bytes()).hexdigest() != HARNESS_SHA256:
            raise RuntimeError("maintained observational source changed")
        config = (HARNESS.parent.parent / "share/nested.kdl").read_bytes()
        if hashlib.sha256(config).hexdigest() != CONFIG_SHA256:
            raise RuntimeError("frozen private Niri configuration changed")
        (CASE / "carrier").mkdir(mode=0o700)  # One-shot; never reuse the held case.
        (CASE / "niri").mkdir(mode=0o700)
        (CASE / "harness-state").mkdir(mode=0o700)
        (CASE / "metrics").mkdir(mode=0o700, exist_ok=True)
        self.write_new(CASE / "niri/nested.kdl", config.decode())
        runner = f'''#!/bin/sh
set -eu
umask 077
while [ ! -f {CASE}/start-runner ]; do sleep 0.1; done
cd {SOURCE}
export PATH=/home/marvin/.rustup/toolchains/1.93.0-x86_64-unknown-linux-gnu/bin:$PATH
export CARGO_TARGET_DIR={TARGET}
export CARGO_BUILD_JOBS=1
export XDG_RUNTIME_DIR={CASE}/runtime
export XDG_DATA_HOME={CASE}/data
export XDG_CONFIG_HOME={CASE}/config
export XDG_CACHE_HOME={CASE}/cache
export CAGENTS_CHAT_CHRONOLOGY_FIXTURE=capacity-chronology-2014
export CAGENTS_CHAT_STAGE_OUTPUT={CASE}/metrics/stages.json
exec scripts/run-development --browserless --debug --log-file {CASE}/ui.log
'''
        self.write_new(CASE / "runner.sh", runner)
        (CASE / "runner.sh").chmod(0o700)
        launcher = f'''#!/bin/sh
umask 077
printf 'WAYLAND_DISPLAY=%s\\nNIRI_SOCKET=%s\\n' "$WAYLAND_DISPLAY" "$NIRI_SOCKET" > {CASE}/niri/session.env
/bin/sh {CASE}/runner.sh >> {CASE}/niri/session.log 2>&1
printf %s "$?" > {CASE}/niri/session.rc
'''
        self.write_new(CASE / "niri/launch.sh", launcher)
        (CASE / "niri/launch.sh").chmod(0o700)

    def spawn(self):
        output = (CASE / "niri/compositor.log").open("xb")
        self.outputs.append(output)
        return subprocess.Popen(
            ["niri", "-c", str(CASE / "niri/nested.kdl"), "--", str(CASE / "niri/launch.sh")],
            cwd=SOURCE, stdout=output, stderr=subprocess.STDOUT, start_new_session=True,
        )  # Natural environment inheritance; no authority/HOME inspection/copy.

    def pin(self, child):
        return os.pidfd_open(child.pid)

    def registration(self):
        path = CASE / "niri/session.env"
        if not path.exists():
            return None
        text = path.read_text()
        if not text.endswith("\n") or len(text.splitlines()) < 2:
            return None
        rows = [line.split("=", 1) for line in text.splitlines()]
        if len(rows) != 2 or any(len(row) != 2 for row in rows):
            raise RuntimeError("invalid private handshake")
        fields = dict(rows)
        if set(fields) != {"NIRI_SOCKET", "WAYLAND_DISPLAY"} or not all(fields.values()):
            raise RuntimeError("invalid private handshake")
        endpoint = Path(fields["NIRI_SOCKET"])
        display = Path(fields["WAYLAND_DISPLAY"])
        if not display.is_absolute():
            display = CASE / "runtime" / display
        if endpoint.resolve().parent != CASE / "runtime" or display.resolve().parent != CASE / "runtime":
            raise RuntimeError("handshake escaped the private runtime")
        return fields

    def bind_peer(self, child, pin, registration):
        # WNOWAIT observes the pinned original; it never substitutes for reap.
        if os.waitid(os.P_PIDFD, pin, os.WEXITED | os.WNOHANG | os.WNOWAIT) is not None:
            raise RuntimeError("original Niri exited before peer binding")
        self.pending_stream = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.pending_stream.settimeout(10)
        self.pending_stream.connect(registration["NIRI_SOCKET"])
        return Peer(self.pending_stream, child.pid, os.getuid())

    def register(self, child, peer, registration):
        one_output(peer.request("Outputs"))
        self.registration_value = dict(registration, child_pid=child.pid)
        value = dict(name=SESSION, display=registration["WAYLAND_DISPLAY"],
                     socket=registration["NIRI_SOCKET"], pid=child.pid, root=str(CASE / "niri"))
        self.write_new(CASE / "harness-state" / (SESSION + ".json"), json.dumps(value))
        self.write_new(CASE / "carrier/owned-registration.json", json.dumps(value))

    def release_runner(self):
        self.write_new(CASE / "start-runner", "registered exact original peer\n")

    def observe_native(self):
        # Preserve the existing observational helper and sole external input route.
        # Do not use start_session, stop_session, healing or Session.alive as reap.
        try:
            h = runpy.run_path(str(HARNESS))
            r = self.registration_value
            session = h["Session"](SESSION, r["WAYLAND_DISPLAY"], r["NIRI_SOCKET"],
                                   r["child_pid"], CASE / "niri")
            h["await_application"](session, CASE / "niri/session.rc", time.monotonic() + 600)
            before = session.niri_json("outputs")
            if len(before) != 1:
                raise RuntimeError("expected one owned output")
            name, output = next(iter(before.items()))
            logical, mode = output["logical"], output["modes"][output["current_mode"]]
            transform = str(logical["transform"]).lower()
            width = mode["height"] if "90" in transform or "270" in transform else mode["width"]
            scale = round(max(0.5, min(4.0, width / 1600)), 3)
            session.niri("output", name, "scale", str(scale))
            time.sleep(0.4)
            after = session.niri_json("outputs")
            geometry = h["output_geometry"](session)
            windows = h["windows"](session)
            paint = h["observe"](session, window_match=None, do_heal=False, with_text=False)
            self.write_new(CASE / "native-window.json", json.dumps(dict(
                session=SESSION, compositor_pid=r["child_pid"], socket=r["NIRI_SOCKET"],
                display=r["WAYLAND_DISPLAY"], fit=dict(physical_width=width,
                requested_scale=scale, before_outputs=before, after_outputs=after),
                logical_geometry=geometry, windows=windows, paint=paint)))
            return 0 if geometry[0] == 1600 and len(windows) == 1 else 1
        except Exception as error:
            self.write_new(CASE / "native-window-error.txt", repr(error))
            return 1

    def runner_status(self):
        path = CASE / "niri/session.rc"
        if not path.exists():
            return None
        value = path.read_text().strip()
        return int(value) if value.isdecimal() and 0 <= int(value) <= 255 else None

    def normal_quit(self, peer):
        return peer.request(QUIT)

    def wait_original(self, child):
        try:
            return child.wait(timeout=0)
        except subprocess.TimeoutExpired:
            return None

    def cancel_requested(self):
        path = CASE / "carrier/cancel-request"
        try:
            info = path.lstat()
        except FileNotFoundError:
            return False
        if not stat.S_ISREG(info.st_mode) or info.st_uid != os.getuid() or info.st_nlink != 1:
            raise RuntimeError("invalid private cancellation request")
        return True  # Never signals; cannot turn an incomplete wait into closure.

    def close_owned(self, peer, pin):
        peer.stream.close()
        self.pending_stream = None
        os.close(pin)
        for output in self.outputs:
            output.close()

    def record(self, state):
        record_observation(state, self)

    def persist_state(self, observation):
        path = CASE / "carrier/state.json"
        temporary = CASE / "carrier/.state-next"
        with temporary.open("w", encoding="utf-8") as stream:
            json.dump(observation, stream, indent=2)
            stream.write("\n")
        temporary.replace(path)

    def publish_state(self, observation):
        print(json.dumps(observation), flush=True)  # Original stream sees HOLD too.

    def pause(self):
        time.sleep(0.2)  # Next tick observes retained originals, not a blind HOLD.


def main():
    owner = Carrier(LivePorts())
    return owner.run()


if __name__ == "__main__":
    raise SystemExit(main())
