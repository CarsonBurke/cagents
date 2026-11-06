"""Linux component checks only; these do not exercise Git or the Rust guard."""
import array
import json
import os
from pathlib import Path
import select
import signal
import socket
import subprocess
import sys
import tempfile
import time
import threading
import unittest
import uuid


@unittest.skipUnless(sys.platform == "linux", "original pidfd component checks require Linux")
class PipeHolderProtocol(unittest.TestCase):
    def setUp(self):
        self.assertEqual(signal.getsignal(signal.SIGCHLD), signal.SIG_DFL,
                         "original Child waits require the unchanged default reaper")
        self.root = tempfile.TemporaryDirectory(prefix="owned-hook-protocol-")
        self.children = []
        self.descriptors = []

    def close(self, fd):
        os.close(fd)
        self.descriptors.remove(fd)

    def pipe(self):
        pair = os.pipe()
        self.descriptors.extend(pair)
        return pair

    def spawn(self, argv, **options):
        child = subprocess.Popen(argv, bufsize=0, **options)
        # Retain the original Child immediately, including pidfd-open failure.
        original = {"child": child, "pidfd": None, "waited": False}
        self.children.append(original)
        original["pidfd"] = os.pidfd_open(child.pid)
        return original

    def exited(self, original, seconds):
        poll = select.poll()
        poll.register(original["pidfd"], select.POLLIN)
        events = poll.poll(int(seconds * 1000))
        self.assertTrue(all(flags == select.POLLIN for _, flags in events))
        return bool(events)

    def wait(self, original, success=True):
        self.assertTrue(self.exited(original, 5), "original child did not exit")
        code = original["child"].wait(timeout=5)
        original["waited"] = True
        if success:
            self.assertEqual(code, 0)
        else:
            self.assertNotEqual(code, 0)

    def receipt(self, original):
        stream = original["child"].stdout
        deadline = time.monotonic() + 5
        line = bytearray()
        while len(line) < 4096:
            remaining = deadline - time.monotonic()
            self.assertGreater(remaining, 0, "original status deadline")
            self.assertTrue(select.select([stream], [], [], remaining)[0])
            byte = stream.read(1)
            self.assertTrue(byte, "original holder closed its status pipe")
            if byte == b"\n":
                return json.loads(line)
            line.extend(byte)
        self.fail("oversized status")

    def holder(self):
        cookie = str(uuid.uuid4())
        address = str(Path(self.root.name) / (cookie + ".sock"))
        script = Path(self.root.name) / (cookie + ".py")
        template = Path(__file__).with_name("owned_hook_pipe_holder.py").read_text()
        script.write_text(template.replace("__ADDRESS_JSON__", json.dumps(address))
                         .replace("__COOKIE_JSON__", json.dumps(cookie)))
        original = self.spawn([sys.executable, "-I", "-B", "-u", str(script), "--holder"],
                              stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                              stderr=subprocess.DEVNULL, process_group=0)
        ready = self.receipt(original)
        self.assertEqual(ready["state"], "ready")
        self.assertEqual(ready["cookie"], cookie)
        self.assertFalse(self.exited(original, 0))
        return original, script, cookie, address

    def assert_eof(self, fd):
        self.assertTrue(select.select([fd], [], [], 5)[0], "capture writer was not closed")
        self.assertEqual(os.read(fd, 1), b"")

    def tearDown(self):
        errors = []

        def record_error(label, error):
            errors.append(label + ": " + repr(error))

        # Request cooperative shutdown for EVERY original before waiting on any.
        for original in self.children:
            child = original["child"]
            try:
                if child.stdin is not None and not child.stdin.closed:
                    child.stdin.close()
            except BaseException as error:
                record_error("control EOF", error)
        for original in reversed(self.children):
            if original["waited"]:
                continue
            try:
                if original["pidfd"] is not None and not self.exited(original, 5):
                    signal.pidfd_send_signal(original["pidfd"], signal.SIGKILL)
                    errors.append("owned rescue used; component check cannot pass")
                    if not self.exited(original, 5):
                        errors.append("original cleanup still pending; retain all bindings")
                original["child"].wait(timeout=5)
                original["waited"] = True
            except BaseException as error:
                record_error("original settlement", error)

        # An unresolved original is a HOLD, never permission to close its pidfd,
        # status pipe or private context. A final original wait may block here;
        # no bounded-completion claim or new numeric signal fallback is made.
        for original in self.children:
            while not original["waited"]:
                try:
                    original["child"].wait()
                    original["waited"] = True
                except BaseException as error:
                    record_error("original wait HOLD", error)
                    # A wait error without a status cannot be repaired by
                    # reopening a PID. Retain the complete original context.
                    hold = threading.Event()
                    while True:
                        try:
                            hold.wait()
                        except BaseException as interrupted:
                            record_error("retained HOLD interrupted", interrupted)

        self.assertTrue(all(original["waited"] for original in self.children))
        # Resource-close errors stay sticky, but cannot prevent other closes.
        for original in self.children:
            if original["pidfd"] is not None:
                try:
                    os.close(original["pidfd"])
                except BaseException as error:
                    record_error("pidfd close after original wait", error)
            for stream in (original["child"].stdin, original["child"].stdout):
                try:
                    if stream is not None and not stream.closed:
                        stream.close()
                except BaseException as error:
                    record_error("original pipe close", error)
        for fd in self.descriptors:
            try:
                os.close(fd)
            except BaseException as error:
                record_error("fixture pipe close", error)
        try:
            self.root.cleanup()
        except BaseException as error:
            record_error("private context removal after all original waits", error)
        self.assertFalse(errors, "original cleanup errors: " + repr(errors))

    def test_real_pipe_handoff_snapshot_and_control_eof(self):
        holder, script, cookie, _ = self.holder()
        out_read, out_write = self.pipe()
        err_read, err_write = self.pipe()
        hook = self.spawn([sys.executable, "-I", "-B", "-u", str(script)],
                          stdin=subprocess.DEVNULL, stdout=out_write, stderr=err_write,
                          process_group=0)
        self.close(out_write)
        self.close(err_write)
        self.wait(hook)
        holding = self.receipt(holder)
        self.assertEqual(holding["state"], "holding")
        self.assertEqual(holding["cookie"], cookie)
        self.assertNotEqual(holding["holder_group"], holding["hook_group"])
        self.assertFalse(select.select([out_read, err_read], [], [], 0)[0])
        holder["child"].stdin.write(("snapshot " + cookie + "\n").encode())
        self.assertEqual(self.receipt(holder), holding)
        self.assertFalse(self.exited(holder, 0))
        holder["child"].stdin.close()
        self.wait(holder)
        self.assert_eof(out_read)
        self.assert_eof(err_read)

    def test_control_eof_before_any_handoff(self):
        holder, _, _, _ = self.holder()
        holder["child"].stdin.close()
        self.wait(holder)

    def test_malformed_peer_frames_refuse_and_close_all_rights(self):
        for case in ("continuation-rights", "trailing-data", "duplicate-key",
                     "wrong-cookie", "aliased-pipe", "missing-right", "excess-right"):
            with self.subTest(case=case):
                holder, _, cookie, address = self.holder()
                pairs = [self.pipe() for _ in range(3)]
                rights = [write for _, write in pairs[:2]]
                identities = [[os.fstat(fd).st_dev, os.fstat(fd).st_ino] for fd in rights]
                request = {"cookie": cookie, "writers": identities, "hook_group": os.getpgrp()}
                if case == "wrong-cookie":
                    request["cookie"] = "wrong"
                if case == "aliased-pipe":
                    rights[1] = rights[0]
                    request["writers"][1] = request["writers"][0]
                payload = json.dumps(request).encode() + b"\n"
                if case == "duplicate-key":
                    payload = b'{"cookie":"wrong",' + payload[1:]
                if case == "missing-right":
                    rights = rights[:1]
                if case == "excess-right":
                    rights.append(pairs[2][1])
                with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as client:
                    client.settimeout(5)
                    client.connect(address)
                    split = len(payload) // 2 if case == "continuation-rights" else len(payload)
                    try:
                        sent = client.sendmsg([payload[:split]], [
                            (socket.SOL_SOCKET, socket.SCM_RIGHTS, array.array("i", rights))])
                        client.sendall(payload[sent:split])
                        if case == "continuation-rights":
                            sent = client.sendmsg([payload[split:]], [
                                (socket.SOL_SOCKET, socket.SCM_RIGHTS, array.array("i", [pairs[2][1]]))])
                            client.sendall(payload[split + sent:])
                        if case == "trailing-data":
                            client.sendall(b"unexpected suffix")
                        client.shutdown(socket.SHUT_WR)
                        self.assertNotEqual(client.recv(3), b"OK\n")
                    except (BrokenPipeError, ConnectionResetError):
                        pass  # Early refusal is still checked by original wait below.
                for _, write in pairs:
                    self.close(write)
                self.wait(holder, success=False)
                for read, _ in pairs:
                    self.assert_eof(read)


if __name__ == "__main__":
    unittest.main()
