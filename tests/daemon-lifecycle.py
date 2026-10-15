#!/usr/bin/env python3
"""Exercise only private daemon instances from a matched development build.

Run under the campaign/development build lock. No provider credentials or real
agents are used. Every signal targets a child process this fixture started.
"""
import argparse
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import time


def wait_until(predicate, timeout=10, description="condition"):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if predicate():
            return
        time.sleep(0.025)
    raise AssertionError(f"Timed out waiting for {description}")


class Instance:
    def __init__(self, root, binaries, *, idle=60, retention=1):
        self.root = root
        self.binaries = binaries
        self.root.mkdir(mode=0o700)
        runtime = self.root / "runtime"
        runtime.mkdir(mode=0o700)
        self.env = os.environ.copy()
        self.env.update({
            "XDG_RUNTIME_DIR": str(runtime),
            "XDG_DATA_HOME": str(self.root / "data"),
            "XDG_CONFIG_HOME": str(self.root / "config"),
            "XDG_STATE_HOME": str(self.root / "state"),
            "DBUS_SESSION_BUS_ADDRESS": f"unix:path={self.root / 'absent-bus'}",
            "CAGENTS_DEVELOPMENT_IDLE_TIMEOUT_SECS": str(idle),
            "CAGENTS_EXITED_TERMINAL_RETENTION_SECS": str(retention),
            "CAGENTS_LOCAL_DEBUG": "1",
            "RUST_LOG": "cagents=debug,cagentsd=debug,warn",
        })
        for key in ("CAGENTS_DEVICE", "CAGENTS_DEVICE_SOCKET", "CAGENTS_DAEMON_FENCE",
                    "CAGENTS_INSTALL_GUARD", "DISPLAY", "WAYLAND_DISPLAY", "WAYLAND_SOCKET"):
            self.env.pop(key, None)
        help_result = subprocess.run([str(binaries / "cagents"), "terminal", "list", "--help"],
                                     env=self.env, capture_output=True, text=True, timeout=10)
        self.list_all = "--all" in help_result.stdout
        self.socket = runtime / "cagents-development" / "cagentsd.sock"
        self.log = open(self.root / "daemon.log", "wb")
        self.process = subprocess.Popen([str(binaries / "cagentsd")], env=self.env,
                                        stdout=self.log, stderr=subprocess.STDOUT)
        wait_until(lambda: self.socket.is_socket(), description="development listener")
        assert self.process.poll() is None, "daemon exited during startup"

    def cli(self, *args):
        result = subprocess.run([str(self.binaries / "cagents"), "--json", *args],
                                env=self.env, capture_output=True, text=True, timeout=15)
        if result.returncode:
            raise AssertionError(f"CLI {args!r} failed: {result.stdout}\n{result.stderr}")
        reply = json.loads(result.stdout)
        assert reply.get("ok", True), reply
        return reply["data"]

    def spawn(self, *command):
        result = self.cli("terminal", "spawn", "--", *command)["terminal"]
        return result.get("id") or result["session"]["id"]

    def terminals(self):
        options = ("--all",) if self.list_all else ()
        return self.cli("terminal", "list", *options)["terminals"]

    def close(self):
        if self.process.poll() is None:
            self.process.terminate()
            try:
                self.process.wait(timeout=10)
            except subprocess.TimeoutExpired:
                self.process.kill()  # exact fixture child, never process discovery
                self.process.wait(timeout=5)
        self.log.close()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--bin-directory", required=True, type=Path)
    parser.add_argument("--evidence", required=True, type=Path)
    args = parser.parse_args()
    binaries = args.bin_directory.resolve()
    for product in ("cagents", "cagentsd"):
        assert (binaries / product).is_file(), f"Missing matched {product}"
    args.evidence.mkdir(parents=True, exist_ok=False)
    checks = []
    with tempfile.TemporaryDirectory(prefix="cagents-lifecycle-", dir="/var/tmp") as temporary:
        root = Path(temporary)
        instance = Instance(root / "retention", binaries)
        try:
            terminal = instance.spawn("/bin/true")
            holder_paths = list(instance.socket.parent.joinpath("holders").glob("*.json"))
            holders = [json.loads(path.read_text())["holder"] for path in holder_paths]
            wait_until(lambda: not instance.terminals(), description="exited terminal retirement")
            for holder in holders:
                wait_until(lambda: not Path(f"/proc/{holder['pid']}").exists(),
                           description="retired terminal holder exit")
            checks.append({"contract": "exited_unattached_terminal_and_holder_retire", "terminal": terminal,
                           "holders": holders})
        finally:
            instance.close()
        instance = Instance(root / "idle", binaries, idle=1)
        try:
            instance.process.wait(timeout=10)
            assert instance.process.returncode == 0
            checks.append({"contract": "empty_development_daemon_exits", "exit_code": 0})
        finally:
            instance.close()
        instance = Instance(root / "busy", binaries, idle=1)
        try:
            terminal = instance.spawn("/bin/sleep", "60")
            time.sleep(2)
            assert instance.process.poll() is None, "idle policy stopped a detached live terminal"
            instance.cli("terminal", "close", terminal)
            instance.process.wait(timeout=10)
            checks.append({"contract": "detached_terminal_prevents_idle_exit_until_closed", "terminal": terminal})
        finally:
            instance.close()
        for busy in (False, True):
            version = root / f"version-{busy}"
            version.mkdir()
            for product in ("cagents", "cagentsd"):
                try:
                    os.link(binaries / product, version / product)
                except OSError:
                    shutil.copy2(binaries / product, version / product)
            instance = Instance(root / f"stale-{busy}", version)
            try:
                terminal = instance.spawn("/bin/sleep", "60") if busy else None
                (version / "cagentsd").unlink()
                result = instance.cli("daemon", "cleanup-stale-development")
                if busy:
                    assert not result["stopped"], result
                    assert instance.process.poll() is None
                    assert instance.terminals(), "cleanup discarded a hosted terminal"
                else:
                    assert result["stopped"], result
                    instance.process.wait(timeout=5)
                    assert instance.process.returncode == 0
                checks.append({"contract": "stale_executable_cleanup", "busy": busy, "stopped": result["stopped"],
                               "terminal": terminal})
            finally:
                instance.close()
        shutil.copytree(root, args.evidence / "instances", ignore=shutil.ignore_patterns("*.sock", "cagents", "cagentsd"))
    (args.evidence / "result.json").write_text(json.dumps({"checks": checks}, indent=2) + "\n")
    print(json.dumps({"passed": len(checks), "evidence": str(args.evidence)}))


if __name__ == "__main__":
    main()
