"""Offline contracts: fake process/socket/native seams, no environment copying.

Run this leaf only after its exact source/custody review. Importing the carrier
does not construct LivePorts. No subprocess, native helper, filesystem fixture,
socket, /proc or environment adapter is used by these contracts.
"""

import importlib.util
from pathlib import Path
import struct
import unittest


spec = importlib.util.spec_from_file_location("fixed_owned_carrier", Path(__file__).with_name("owned_carrier.py"))
carrier = importlib.util.module_from_spec(spec)
spec.loader.exec_module(carrier)


class FakePorts:
    def __init__(self, ack="Handled"):
        self.child = type("FakeOriginalChild", (), {"pid": 41})()
        self.pin_value, self.peer = object(), object()
        self.events = []
        self.ack = ack
        self.status = None
        self.child_status = None
        self.cancel = False
        self.bind_error = self.pin_error = self.wait_error = None
        self.record_error = False
        self.pause_count = 0

    def prepare(self):
        self.events.append("prepare")

    def spawn(self):
        self.events.append("spawn-original")
        return self.child

    def pin(self, child):
        assert child is self.child
        if self.pin_error:
            raise self.pin_error
        return self.pin_value

    def registration(self):
        return {"private": "fake-handshake"}

    def bind_peer(self, child, pin, registration):
        assert child is self.child and pin is self.pin_value
        if self.bind_error:
            raise self.bind_error
        self.events.append("bind-original-peer")
        return self.peer

    def register(self, child, peer, registration):
        assert child is self.child and peer is self.peer
        self.events.append("register-private")

    def release_runner(self):
        self.events.append("release-once")

    def observe_native(self):
        self.events.append("fake-native-observation")
        return 0

    def runner_status(self):
        return self.status

    def normal_quit(self, peer):
        assert peer is self.peer
        self.events.append("quit-same-peer")
        if isinstance(self.ack, BaseException):
            raise self.ack
        return self.ack

    def wait_original(self, child):
        assert child is self.child
        self.events.append("wait-original")
        if self.wait_error:
            raise self.wait_error
        return self.child_status

    def cancel_requested(self):
        return self.cancel

    def close_owned(self, peer, pin):
        assert peer is self.peer and pin is self.pin_value
        self.events.append("close-after-wait")

    def record(self, state):
        if self.record_error and state["cleanup_complete"]:
            self.record_error = False
            raise OSError("fake evidence failure")

    def pause(self):
        self.pause_count += 1
        if self.pause_count == 1:
            self.status = 0
        elif self.pause_count == 2:
            self.child_status = 0


class FakeStream:
    def __init__(self, reply=b'{"Ok":"Handled"}\n', pid=41):
        self.reply = bytearray(reply)
        self.pid = pid
        self.sent = []

    def getsockopt(self, *unused):
        return struct.pack("3i", self.pid, 1000, 1000)

    def sendall(self, data):
        self.sent.append(data)

    def recv(self, count):
        result = bytes(self.reply[:count])
        del self.reply[:count]
        return result


class CustodyContracts(unittest.TestCase):
    def started(self, ports):
        owner = carrier.Carrier(ports)
        owner.start()
        owner.step()
        return owner

    def finish(self, owner, ports):
        ports.status = 0
        owner.step()
        self.assertFalse(owner.state["cleanup_complete"])
        ports.child_status = 0
        owner.step()

    def test_original_wait_precedes_close_and_only_one_release_quit(self):
        ports = FakePorts()
        owner = self.started(ports)
        self.finish(owner, ports)
        self.assertTrue(owner.state["cleanup_complete"])
        self.assertEqual(owner.state["exit_code"], 0)
        self.assertEqual(ports.events.count("release-once"), 1)
        self.assertEqual(ports.events.count("quit-same-peer"), 1)
        self.assertEqual(ports.events[-2:], ["wait-original", "close-after-wait"])

    def test_quit_eof_still_waits_reaps_and_never_claims_ack(self):
        ports = FakePorts(carrier.MissingAck("fake normal-quit EOF"))
        owner = self.started(ports)
        ports.status = 0
        owner.step()
        for _ in range(3):
            owner.step()
        self.assertEqual(owner.state["quit_ack"], "missing-eof")
        self.assertFalse(owner.state["child_wait_complete"])
        self.assertNotIn("close-after-wait", ports.events)
        self.assertEqual(ports.events.count("quit-same-peer"), 1)
        ports.child_status = 0
        owner.step()
        self.assertTrue(owner.state["child_wait_complete"])
        self.assertEqual(owner.state["child_status"], 0)
        self.assertTrue(owner.state["cleanup_complete"])
        self.assertEqual(owner.state["quit_ack"], "missing-eof")

    def test_wrong_peer_holds_original_without_releasing_or_quitting(self):
        ports = FakePorts()
        ports.bind_error = RuntimeError("wrong peer")
        owner = self.started(ports)
        self.assertIs(owner.child, ports.child)
        self.assertIs(owner.pin, ports.pin_value)
        self.assertEqual(owner.state["phase"], "HOLD")
        ports.child_status = 0
        owner.step()
        self.assertTrue(owner.state["child_wait_complete"])
        self.assertFalse(owner.state["cleanup_complete"])
        self.assertNotIn("release-once", ports.events)
        self.assertNotIn("quit-same-peer", ports.events)
        self.assertNotIn("close-after-wait", ports.events)

    def test_pin_failure_retains_original_and_observes_its_wait(self):
        ports = FakePorts()
        ports.pin_error = OSError("fake pidfd failure")
        owner = self.started(ports)
        self.assertIs(owner.child, ports.child)
        self.assertEqual(owner.state["phase"], "HOLD")
        self.assertIn("wait-original", ports.events)
        self.assertNotIn("release-once", ports.events)

    def test_wait_error_holds_all_original_handles_without_cleanup(self):
        ports = FakePorts()
        owner = self.started(ports)
        ports.wait_error = OSError("fake unknown original wait")
        owner.step()
        self.assertIs(owner.child, ports.child)
        self.assertIs(owner.peer, ports.peer)
        self.assertIs(owner.pin, ports.pin_value)
        self.assertFalse(owner.state["child_wait_complete"])
        self.assertEqual(owner.state["phase"], "HOLD")
        self.assertNotIn("close-after-wait", ports.events)

    def test_cancel_cannot_quit_before_runner_return_or_manufacture_closure(self):
        ports = FakePorts()
        owner = self.started(ports)
        ports.cancel = True
        owner.step()
        self.assertNotIn("quit-same-peer", ports.events)
        self.assertFalse(owner.state["cleanup_complete"])
        self.finish(owner, ports)
        self.assertEqual(owner.state["exit_code"], 2)
        self.assertTrue(owner.state["cancelled"])
        self.assertTrue(owner.state["child_wait_complete"])

    def test_non_eof_quit_error_preserves_negative_but_still_waits(self):
        ports = FakePorts(OSError("fake lost transport"))
        owner = self.started(ports)
        self.finish(owner, ports)
        self.assertTrue(owner.state["cleanup_complete"])
        self.assertEqual(owner.state["exit_code"], 2)
        self.assertEqual(owner.state["quit_ack"], "error")

    def test_nonzero_original_exit_stays_negative_hold(self):
        ports = FakePorts()
        owner = self.started(ports)
        ports.status = 0
        owner.step()
        ports.child_status = 7
        owner.step()
        self.assertEqual(owner.state["child_status"], 7)
        self.assertEqual(owner.state["phase"], "HOLD")
        self.assertFalse(owner.state["cleanup_complete"])

    def test_real_loop_body_progresses_original_after_eof_without_blind_hold(self):
        ports = FakePorts(carrier.MissingAck("fake EOF"))
        owner = carrier.Carrier(ports)
        owner.start()
        for _ in range(3):
            owner.turn()
        self.assertTrue(owner.state["cleanup_complete"])
        self.assertEqual(owner.state["exit_code"], 0)
        self.assertEqual(ports.pause_count, 2)
        self.assertTrue(owner.state["child_wait_complete"])

    def test_evidence_error_revokes_clean_completion_and_keeps_owner(self):
        ports = FakePorts()
        ports.record_error = True
        owner = carrier.Carrier(ports)
        owner.start()
        for _ in range(3):
            owner.turn()
        self.assertIs(owner.child, ports.child)
        self.assertEqual(owner.state["phase"], "HOLD")
        self.assertFalse(owner.state["cleanup_complete"])
        self.assertIsNone(owner.state["exit_code"])


class PeerContracts(unittest.TestCase):
    def test_normal_quit_wire_uses_existing_bound_stream(self):
        stream = FakeStream()
        peer = carrier.Peer(stream, 41, 1000)
        self.assertEqual(peer.request(carrier.QUIT), "Handled")
        self.assertEqual(stream.sent, [b'{"Action":{"Quit":{"skip_confirmation":true}}}\n'])

    def test_mismatched_peer_cannot_receive_quit(self):
        stream = FakeStream(pid=42)
        with self.assertRaises(RuntimeError):
            carrier.Peer(stream, 41, 1000)
        self.assertEqual(stream.sent, [])

    def test_eof_and_partial_reply_are_not_an_ack(self):
        for reply in [b"", b'{"Ok":']:
            with self.subTest(reply=reply):
                peer = carrier.Peer(FakeStream(reply), 41, 1000)
                with self.assertRaises(carrier.MissingAck):
                    peer.request(carrier.QUIT)


if __name__ == "__main__":
    unittest.main()
