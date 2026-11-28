#!/usr/bin/env python3
"""Exercise the Swift transport against an isolated OpenSSH and cagents daemon."""

import argparse
import base64
import json
import os
from pathlib import Path
import secrets
import selectors
import socket
import subprocess
import tempfile
import time
import urllib.parse


ROOT = Path(__file__).resolve().parents[2]
FIXTURE = ROOT / "scripts/mobile-pair-fixture"


def fixture_command(environment, *args):
    return subprocess.run(
        [FIXTURE, *args], env=environment, check=True, stdout=subprocess.PIPE,
        text=True, timeout=20,
    ).stdout


def local_port():
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return str(sock.getsockname()[1])


def make_client_key(probe, directory, algorithm, filename=None):
    path = directory / (filename or algorithm)
    # Both algorithms take 32 bytes. This also keeps the P-256 scalar below its order.
    path.write_bytes(b"\x01" + secrets.token_bytes(31))
    public = subprocess.check_output([probe, "public-key", algorithm, path], text=True, timeout=10).strip()
    return path, public


class Bridge:
    def __init__(self, command, environment):
        self.process = subprocess.Popen(
            command, env=environment, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
        )
        self.buffer = bytearray()

    def __enter__(self):
        return self

    def __exit__(self, exc_type, *_):
        if exc_type is not None:
            self.process.kill()
        try:
            _, error = self.process.communicate(timeout=5)
        except subprocess.TimeoutExpired:
            self.process.kill()
            self.process.communicate(timeout=5)
            raise
        if exc_type is None and self.process.returncode:
            raise RuntimeError(error.decode())

    def send(self, message):
        self.process.stdin.write(json.dumps(message).encode() + b"\n")
        self.process.stdin.flush()

    def expect(self, kind, **fields):
        end = time.monotonic() + 10
        while True:
            if b"\n" in self.buffer:
                line, _, self.buffer = self.buffer.partition(b"\n")
                message = json.loads(line)
                if message["type"] == kind:
                    assert all(message.get(key) == value for key, value in fields.items()), message
                    return
                if message["type"] != "event":
                    raise AssertionError(message)
                continue
            with selectors.DefaultSelector() as ready:
                ready.register(self.process.stdout, selectors.EVENT_READ)
                if not ready.select(max(0, end - time.monotonic())):
                    raise TimeoutError(f"waiting for {kind}")
            chunk = os.read(self.process.stdout.fileno(), 8192)
            if not chunk:
                _, error = self.process.communicate(timeout=5)
                raise RuntimeError(f"bridge closed while waiting for {kind}: {error.decode()}")
            self.buffer.extend(chunk)


def mobile_exchange(command, environment):
    with Bridge(command, environment) as bridge:
        bridge.send({
            "type": "hello", "api_min": 1, "api_max": 1,
            "client": {"name": "cagents-ios-fixture", "version": "1"},
        })
        bridge.expect("welcome", api=1)
        bridge.send({"type": "request", "id": 1, "op": "ping"})
        bridge.expect("ok", id=1)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("probe", type=lambda path: str(Path(path).resolve()))
    parser.add_argument("bins", type=lambda path: str(Path(path).resolve()))
    parser.add_argument("--shared", action="store_true", help="also run the shared JVM wire test on fw16")
    parser.add_argument("--pair", action="store_true", help="also pair using a computer build that accepts P-256")
    args = parser.parse_args()

    with tempfile.TemporaryDirectory(prefix="cagents-ios-fixture-", dir="/var/tmp") as private:
        directory = Path(private)
        port = local_port()
        env = dict(os.environ, CAGENTS_PAIR_FIXTURE=str(directory / "host"))
        try:
            fixture_command(env, "start", args.bins, "--host", "127.0.0.1", "--port", port)
            pin = subprocess.check_output(
                ["ssh-keygen", "-lf", directory / "host/host_key.pub"], text=True, timeout=10,
            ).split()[1]
            env["FIXTURE_HOST_PIN"] = pin
            commands = {}
            for algorithm in ("ed25519", "p256"):
                path, key = make_client_key(args.probe, directory, algorithm)
                fixture_command(env, "authorize", key)
                commands[algorithm] = [
                    args.probe, "127.0.0.1", port, env["USER"], algorithm, "--mobile", str(path),
                ]
            mobile_exchange(commands["ed25519"], env)
            samples = {algorithm: [] for algorithm in commands}
            for _ in range(10):
                for algorithm, command in commands.items():
                    start = time.monotonic()
                    mobile_exchange(command, env)
                    samples[algorithm].append(round((time.monotonic() - start) * 1000))

            command = commands["p256"]
            wrong_pin = subprocess.run(
                command, env=dict(env, FIXTURE_HOST_PIN=pin + "x"), input="", capture_output=True,
                text=True, timeout=12,
            )
            assert wrong_pin.returncode and "hostKeyChanged" in wrong_pin.stderr, wrong_pin.stderr
            other_path = directory / "wrong-key"
            other_path.write_bytes(b"\x02" + secrets.token_bytes(31))
            wrong_key = subprocess.run(
                [*command[:-1], other_path], env=env, input="", capture_output=True,
                text=True, timeout=12,
            )
            assert wrong_key.returncode and "unauthorized" in wrong_key.stderr, wrong_key.stderr
            print(f"Mobile API v1 welcome and ping, milliseconds per connection: {samples}", flush=True)
            print("Host pin mismatch and unpaired key refused", flush=True)

            if args.pair:
                code = fixture_command(env, "code").strip()
                parameters = urllib.parse.parse_qs(urllib.parse.urlsplit(code).query)
                seed = directory / "one-time-seed"
                encoded = parameters["key"][0]
                seed.write_bytes(base64.urlsafe_b64decode(encoded + "=" * (-len(encoded) % 4)))
                paired_path, phone_key = make_client_key(args.probe, directory, "p256", "paired-p256")
                with Bridge([*command[:4], "ed25519", "--pair", str(seed)], env) as bridge:
                    bridge.send({"type": "pair", "key": phone_key, "name": "Fixture phone"})
                    bridge.expect("pair_waiting")
                    fixture_command(env, "allow")
                    bridge.expect("paired")
                contents = (directory / "host/authorized_keys").read_text()
                expected = f'/cagents daemon bridge --mobile",restrict {phone_key} Fixture phone'
                assert sum(line.endswith(expected) for line in contents.splitlines()) == 1
                assert " daemon bridge --pair " not in contents
                command = [*command[:-1], str(paired_path)]
                mobile_exchange(command, env)
                print("One-time Ed25519 exchange authorized a P-256 key that reached API v1", flush=True)

            if args.shared:
                if socket.gethostname().split(".")[0] != "fw16":
                    raise SystemExit("--shared runs Gradle on fw16")
                subprocess.run(
                    ["./gradlew", "--console=plain", ":shared:jvmTest", "--tests",
                     "dev.cagents.mobile.NativeSshFixtureTest", "--rerun"],
                    cwd=ROOT / "mobile", check=True,
                    env=dict(env, CAGENTS_GRADLE_LOCAL="1", CAGENTS_TEST_SSH_COMMAND="\n".join(command)),
                    timeout=240,
                )
        finally:
            fixture_command(env, "stop")


if __name__ == "__main__":
    main()
