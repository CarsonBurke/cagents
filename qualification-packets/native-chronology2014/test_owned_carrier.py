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
        self.prepare_error = self.spawn_error = None
        self.outputs_reply = {"Outputs": {"fake-owned-output": {}}}
        self.record_error = False
        self.publish_error = False
        self.pause_count = 0
        self.records = []
        self.persisted = []

    def prepare(self):
        self.events.append("prepare")
        if self.prepare_error:
            raise self.prepare_error

    def spawn(self):
        self.events.append("spawn-original")
        if self.spawn_error:
            raise self.spawn_error
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
        carrier.one_output(self.outputs_reply)
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
        carrier.record_observation(state, self)

    def persist_state(self, observation):
        if self.record_error and observation["cleanup_complete"]:
            self.record_error = False
            raise OSError("fake evidence failure")
        self.persisted.append(dict(observation))

    def publish_state(self, observation):
        if self.publish_error and observation["cleanup_complete"]:
            self.publish_error = False
            raise OSError("fake original-stream failure")
        self.records.append(dict(observation))

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
        self.assertEqual(owner.state["exit_code"], 2)

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

    def test_nonzero_original_exit_settles_negative_after_owned_close(self):
        ports = FakePorts()
        owner = self.started(ports)
        ports.status = 0
        owner.step()
        ports.child_status = 7
        owner.step()
        self.assertEqual(owner.state["child_status"], 7)
        self.assertEqual(owner.state["phase"], "owned-cleanup-settled")
        self.assertTrue(owner.state["cleanup_complete"])
        self.assertEqual(owner.state["exit_code"], 2)
        self.assertEqual(ports.events[-2:], ["wait-original", "close-after-wait"])

    def test_real_loop_body_progresses_original_after_eof_without_blind_hold(self):
        ports = FakePorts(carrier.MissingAck("fake EOF"))
        owner = carrier.Carrier(ports)
        owner.start()
        for _ in range(3):
            owner.turn()
        self.assertTrue(owner.state["cleanup_complete"])
        self.assertEqual(owner.state["exit_code"], 2)
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
        self.assertTrue(owner.state["owned_close_complete"])
        self.assertTrue(owner.state["child_wait_complete"])
        self.assertEqual(ports.events.count("close-after-wait"), 1)
        self.assertFalse(any(record["cleanup_complete"] for record in ports.records))
        self.assertFalse(any(record["cleanup_complete"] for record in ports.persisted))
        owner.turn()
        self.assertEqual(ports.events.count("close-after-wait"), 1)
        self.assertTrue(ports.records[-1]["owned_close_complete"])
        self.assertFalse(ports.records[-1]["cleanup_complete"])

    def test_stream_failure_after_persistence_never_publishes_terminal_success(self):
        ports = FakePorts()
        ports.publish_error = True
        owner = carrier.Carrier(ports)
        owner.start()
        for _ in range(3):
            owner.turn()
        self.assertEqual(owner.state["phase"], "HOLD")
        self.assertTrue(owner.state["owned_close_complete"])
        self.assertTrue(owner.state["child_wait_complete"])
        self.assertIsNone(owner.state["exit_code"])
        self.assertFalse(any(record["cleanup_complete"] for record in ports.records))
        candidate = ports.persisted[-1]
        self.assertTrue(candidate["owned_close_complete"])
        self.assertEqual(candidate["proposed_exit_code"], 0)
        self.assertFalse(candidate["terminal_return_confirmed"])
        self.assertNotIn("exit_code", candidate)
        owner.turn()
        self.assertEqual(ports.events.count("close-after-wait"), 1)
        self.assertEqual(ports.records[-1]["phase"], "HOLD")
        self.assertFalse(ports.records[-1]["terminal_return_confirmed"])

    def test_prepare_refusal_returns_recorded_negative_without_spawn(self):
        ports = FakePorts()
        ports.prepare_error = OSError("fake private preparation refusal")
        owner = carrier.Carrier(ports)
        self.assertEqual(owner.run(), 2)
        self.assertEqual(ports.events, ["prepare"])
        self.assertTrue(owner.state["pre_spawn_refused"])
        self.assertIsNone(owner.child)
        self.assertFalse(owner.state["child_wait_complete"])
        self.assertEqual(ports.records[-1]["proposed_exit_code"], 2)
        self.assertFalse(ports.records[-1]["terminal_return_confirmed"])
        self.assertEqual(ports.persisted, [])

    def test_spawn_uncertainty_cannot_be_promoted_to_no_child_refusal(self):
        ports = FakePorts()
        ports.spawn_error = OSError("fake ambiguous spawn failure")
        owner = carrier.Carrier(ports)
        owner.start()
        owner.owner_turn()
        self.assertEqual(owner.state["phase"], "HOLD")
        self.assertFalse(owner.state["pre_spawn_refused"])
        self.assertFalse(owner.state["cleanup_complete"])
        self.assertIsNone(owner.state["exit_code"])

    def test_no_spawn_refusal_stays_negative_if_diagnostic_write_fails(self):
        ports = FakePorts()
        ports.prepare_error = OSError("fake private preparation refusal")
        ports.publish_error = True
        owner = carrier.Carrier(ports)
        self.assertEqual(owner.run(), 2)
        self.assertEqual(ports.events, ["prepare"])
        self.assertTrue(owner.state["pre_spawn_refused"])
        self.assertFalse(owner.state["blocked"])
        self.assertTrue(any("evidence:" in item for item in owner.state["errors"]))

    def test_interruption_at_turn_boundary_retains_owner_and_observes_original(self):
        ports = FakePorts()
        owner = self.started(ports)
        original_turn = owner.turn

        def interrupted_turn():
            raise KeyboardInterrupt("fake interruption outside adapter catches")

        owner.turn = interrupted_turn
        owner.owner_turn()
        self.assertEqual(owner.state["phase"], "HOLD")
        self.assertIs(owner.child, ports.child)
        self.assertIs(owner.peer, ports.peer)
        self.assertIs(owner.pin, ports.pin_value)
        owner.turn = original_turn
        ports.child_status = 0
        owner.owner_turn()
        self.assertTrue(owner.state["child_wait_complete"])
        self.assertEqual(ports.events.count("spawn-original"), 1)
        self.assertFalse(owner.state["cleanup_complete"])
        self.assertNotIn("close-after-wait", ports.events)

    def test_invalid_output_variant_or_count_cannot_release_runner(self):
        for response in [{"Outputs": {}}, {"Outputs": {"a": {}, "b": {}}},
                         {"Windows": []}, {"Outputs": []}]:
            with self.subTest(response=response):
                ports = FakePorts()
                ports.outputs_reply = response
                owner = self.started(ports)
                self.assertEqual(owner.state["phase"], "HOLD")
                self.assertNotIn("register-private", ports.events)
                self.assertNotIn("release-once", ports.events)
                self.assertNotIn("quit-same-peer", ports.events)


class PeerContracts(unittest.TestCase):
    def test_actual_outputs_reply_is_unwrapped_and_counted(self):
        stream = FakeStream(b'{"Ok":{"Outputs":{"owned":{"logical":{}}}}}\n')
        peer = carrier.Peer(stream, 41, 1000)
        self.assertEqual(carrier.one_output(peer.request("Outputs")), {"owned": {"logical": {}}})
        self.assertEqual(stream.sent, [b'"Outputs"\n'])

    def test_actual_outputs_parser_rejects_empty_multiple_or_wrong_variant(self):
        for reply in [b'{"Ok":{"Outputs":{}}}\n',
                      b'{"Ok":{"Outputs":{"a":{},"b":{}}}}\n',
                      b'{"Ok":{"Windows":[]}}\n']:
            with self.subTest(reply=reply):
                peer = carrier.Peer(FakeStream(reply), 41, 1000)
                with self.assertRaises(RuntimeError):
                    carrier.one_output(peer.request("Outputs"))

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
