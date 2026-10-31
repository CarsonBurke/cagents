import ast
from pathlib import Path
from types import SimpleNamespace
import unittest


class AdoptedSettlementPolicy(unittest.TestCase):
    def selected(self, snapshots):
        source = Path(__file__).with_name("settle_children.py").read_text()
        tree = ast.parse(source)
        self.assertEqual(len(tree.body), 1)
        self.assertIsInstance(tree.body[0], ast.FunctionDef)
        self.assertEqual(tree.body[0].name, "settle_children")
        calls = []
        clock = SimpleNamespace(value=0.0)
        def monotonic():
            clock.value += 0.25
            return clock.value
        def sleep(seconds):
            clock.value += seconds
        def forbidden(*args, **kwargs):
            calls.append((args, kwargs))
            raise AssertionError("unproven adoption must not open a pidfd or send a signal")
        def waitpid(*args):
            raise ChildProcessError()
        def children():
            if isinstance(snapshots, Exception):
                raise snapshots
            if len(snapshots) > 1:
                return snapshots.pop(0)
            return snapshots[0]
        namespace = {"os": SimpleNamespace(waitpid=waitpid, WNOHANG=1, pidfd_open=forbidden, close=forbidden),
                     "time": SimpleNamespace(monotonic=monotonic, sleep=sleep),
                     "children": children, "signal_pidfd": forbidden,
                     "signal": SimpleNamespace(SIGKILL=9)}
        exec(compile(tree, "<actual-settlement-policy-definition>", "exec"), namespace)
        return namespace["settle_children"], calls

    def test_surviving_unproven_adoption_is_unsignaled_and_unclean(self):
        child = {"pid": 99999991, "start": 17, "state": "S"}
        settle, calls = self.selected([[child]])
        result = settle()
        self.assertFalse(result["clean"])
        self.assertFalse(result["forced"])
        self.assertEqual(result["observed"], [child])
        self.assertEqual(result["remaining"], [child])
        self.assertEqual(calls, [])

    def test_no_remaining_children_keeps_the_existing_clean_result(self):
        settle, calls = self.selected([[]])
        self.assertEqual(settle(), {"clean": True, "forced": False, "observed": []})
        self.assertEqual(calls, [])

    def test_natural_settlement_within_grace_sends_no_signal(self):
        child = {"pid": 99999991, "start": 17, "state": "S"}
        settle, calls = self.selected([[child], []])
        result = settle()
        self.assertTrue(result["clean"])
        self.assertFalse(result["forced"])
        self.assertEqual(calls, [])

    def test_failed_census_propagates_without_a_clean_or_signal_result(self):
        settle, calls = self.selected(PermissionError("census unavailable"))
        with self.assertRaises(PermissionError):
            settle()
        self.assertEqual(calls, [])


if __name__ == "__main__":
    unittest.main()
