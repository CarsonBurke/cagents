"""Authored UNRUN owning source oracles; never native/API/capture qualification.

Future separately authorized pure execution selects actual parser/key AST only;
no native_owner module, constructor, IO/process/entry method or old harness runs.
This file was AST-parsed only during source preparation.
"""

import ast
import copy
from dataclasses import dataclass, field
import hashlib
import json
import math
from pathlib import Path
import re
import struct
from types import SimpleNamespace
import unittest
import uuid


EXPECTED_OWNER_SHA256 = "a11744d28b08b3c1b28ce3a79fd0bd783012f5510894d36065cc00603605af1e"
MAX_U64 = (1 << 64) - 1
QUERY_A = "11111111-2222-4333-8444-555555555555"
QUERY_B = "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee"


def select_actual_source():
    """Future pure invocation only, using unchanged actual selected bodies."""
    raw = Path(__file__).with_name("native_owner.py").read_bytes()
    if hashlib.sha256(raw).hexdigest() != EXPECTED_OWNER_SHA256:
        raise AssertionError("owning source changed; explicit new source review required")
    tree = ast.parse(raw)
    owner = next(node for node in tree.body if isinstance(node, ast.ClassDef)
                 and node.name == "NativeHistoryOwner")
    methods = {"_history_json", "_history_float", "_history_appearance", "_history_adapter", "_history_log_observers"}
    selected = [copy.deepcopy(node) for node in tree.body
                if isinstance(node, ast.Assign) and any(isinstance(target, ast.Name)
                   and target.id == "APPEARANCE_SOURCES" for target in node.targets)
                or isinstance(node, ast.ClassDef) and node.name in ("Hold", "FrameObservation")
                or isinstance(node, ast.FunctionDef) and node.name == "require"]
    cls = copy.deepcopy(owner)
    cls.body = [copy.deepcopy(node) for node in owner.body
                if isinstance(node, ast.FunctionDef) and node.name in methods]
    if len(cls.body) != len(methods):
        raise AssertionError("actual pure parser selection incomplete")
    selected.append(cls)
    frame = next(node for node in owner.body if isinstance(node, ast.FunctionDef)
                 and node.name == "history_frame_observation")
    assignments = [copy.deepcopy(node) for node in frame.body if isinstance(node, ast.Assign)
                   and len(node.targets) == 1 and isinstance(node.targets[0], ast.Name)
                   and node.targets[0].id in ("stable_appearance", "native")]
    if [node.targets[0].id for node in assignments] != ["stable_appearance", "native"]:
        raise AssertionError("actual capture key selection changed")
    key = ast.parse("def selected_actual_frame_key(binding, appearance, theme):\n    pass\n").body[0]
    key.body = assignments + [ast.Return(value=ast.Tuple(elts=[ast.Name(id="stable_appearance", ctx=ast.Load()),
                                                             ast.Name(id="native", ctx=ast.Load())], ctx=ast.Load()))]
    selected.append(key)
    loop = next(node for node in frame.body if isinstance(node, ast.While))
    ready = next(node for node in loop.body if isinstance(node, ast.If)
                 and len(node.body) == 1 and isinstance(node.body[0], ast.Break))
    predicate = ast.parse("def selected_actual_ready(appearances, adapters, pending):\n    pass\n").body[0]
    predicate.body = [ast.Return(value=copy.deepcopy(ready.test))]
    selected.append(predicate)
    namespace = {"__name__": __name__, "dataclass": dataclass, "field": field,
                 "json": json, "math": math, "re": re, "struct": struct, "uuid": uuid}
    module = ast.Module(body=selected, type_ignores=[])
    # Reparenting exact semantic AST changes no body; normalize only diagnostic
    # locations so copied statements cannot inherit invalid wrapper ranges.
    for node in ast.walk(module):
        if "lineno" in node._attributes:
            node.lineno = node.end_lineno = 1
            node.col_offset = node.end_col_offset = 0
    module = ast.fix_missing_locations(module)
    exec(compile(module, "<actual-reviewed-scale-consumer-selection>", "exec"), namespace)
    return namespace


class InitialNativeScaleSourceOracles(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.selected = select_actual_source()

    def setUp(self):
        self.owner = object.__new__(self.selected["NativeHistoryOwner"])
        self.owner.sources = {"after": {"renderer_build": {"product_version": "pure-source-test"}}}
        self.binding = SimpleNamespace(arm="after", ui_pid=4242, xid=72, generation="pure-owned-generation",
                                      fixture=SimpleNamespace(manifest_sha256="ab" * 32, dataset="small"))
        self.hold = self.selected["Hold"]

    def record(self, **changes):
        value = {"record_version": 1, "record": "history-native-appearance",
                 "observer_instance": "22222222-3333-4444-8555-666666666666", "observation_generation": 1,
                 "pid": 4242, "window_id_kind": "iced_main_window_debug", "window_id": "Id(1)",
                 "window_open_source": "original_window_opened_event", "window_open_generation": 1,
                 "product_version": "pure-source-test", "compiled_source_sha256": self.selected["APPEARANCE_SOURCES"]["after"],
                 "data": {"manifest_sha256": "ab" * 32, "dataset": "small"}, "theme_kind": "dark",
                 "theme_is_dark": True, "theme_is_high_contrast": False, "theme_is_exact_dark_class": True,
                 "applied_ui_scale": 1.0, "applied_ui_scale_bits": 1065353216,
                 "applied_ui_scale_source": "Core::scale_factor_as_returned_by_Shell",
                 "native_scale": None, "native_scale_bits": None, "native_scale_source": "unknown",
                 "native_scale_generation": 0, "native_scale_query_request": None,
                 "state": {"workspace_generation": 1, "source_generation": 1, "inspector_open": False,
                           "history": {"source_state": "ready", "head_oid": "a" * 40, "rows": 200,
                                       "next_page": 2, "loading": False, "selected_commit": None}},
                 "observation_boundary": "view_state_readback_not_compositor_present"}
        value.update(changes)
        return value

    def query(self, **changes):
        value = self.record(native_scale=1.5, native_scale_bits=1069547520,
                            native_scale_source="window_scale_factor_query", native_scale_generation=7,
                            native_scale_query_request=QUERY_A)
        value.update(changes)
        return value

    def scan(self, records):
        raw = b"".join(("2026-10-31T00:00:00.000000Z  INFO cagents::native_history: "
                        "history native appearance state appearance_json=" + json.dumps(record) + "\n").encode()
                       for record in records)
        return self.owner._history_log_observers(self.binding, raw)[0]

    def frame(self, record, evidence):
        self.owner._history_appearance(self.binding, record, record["observation_generation"])
        stable, native = self.selected["selected_actual_frame_key"](self.binding, record, "dark")
        return self.selected["FrameObservation"]((0, 0, 900, 700), "pure-selected-gl", "dark", 1.0,
                                                  json.dumps(evidence), stable, native)

    def test_exact_query_rescaled_and_reopened_unknown_source_records(self):
        unknown = self.record()
        self.assertIsNone(self.owner._history_appearance(self.binding, unknown, 1)["native_scale"])
        query = self.query()
        self.assertEqual(self.owner._history_appearance(self.binding, query, 1)["native_scale"], 1.5)
        rescaled = self.query(native_scale_source="rescaled_event", native_scale_query_request=None)
        self.assertEqual(self.owner._history_appearance(self.binding, rescaled, 1)["native_scale_source"], "rescaled_event")
        superseding = self.query(observation_generation=2, native_scale_source="rescaled_event",
                                 native_scale_generation=8, native_scale_query_request=None)
        self.assertEqual(len(self.scan([query, superseding])), 2)
        reopened = self.record(observation_generation=2, window_open_generation=2, native_scale_generation=7)
        self.assertEqual(len(self.scan([query, reopened])), 2)
        self.assertIsNone(reopened["native_scale"])
        saturated = self.query(window_open_generation=MAX_U64, native_scale_generation=MAX_U64)
        self.assertEqual(self.owner._history_appearance(self.binding, saturated, 1)["native_scale_generation"], MAX_U64)

    def test_malformed_scale_and_lifecycle_provenance_is_refused(self):
        changes = [{"window_open_generation": value} for value in (False, 0, -1, MAX_U64 + 1)]
        changes += [{"native_scale_generation": value} for value in (True, -1, MAX_U64 + 1)]
        changes += [{"native_scale_source": "configured"}, {"native_scale_query_request": None},
                    {"native_scale_query_request": "00000000-0000-0000-0000-000000000000"},
                    {"native_scale_query_request": QUERY_B.upper()}, {"native_scale_query_request": True},
                    {"native_scale": 1.0}, {"native_scale": 0.0}, {"native_scale_bits": None},
                    {"native_scale_generation": 0}, {"native_scale_source": "rescaled_event"},
                    {"native_scale_source": "unknown"}]
        for delta in changes:
            with self.subTest(delta=delta), self.assertRaises(self.hold):
                self.owner._history_appearance(self.binding, self.query(**delta), 1)
        for delta in [{"native_scale_query_request": QUERY_A}, {"native_scale": 1.0, "native_scale_bits": 1065353216}]:
            with self.subTest(unknown=delta), self.assertRaises(self.hold):
                self.owner._history_appearance(self.binding, self.record(**delta), 1)
        missing = self.query()
        missing.pop("native_scale_query_request")
        with self.assertRaises(self.hold):
            self.owner._history_appearance(self.binding, missing, 1)

    def test_stale_query_native_and_opened_sequences_are_refused(self):
        first = self.query()
        changes = [{"window_open_generation": 0}, {"native_scale_generation": 6},
                   {"native_scale_generation": 8},
                   {"native_scale_query_request": QUERY_B},
                   {"native_scale_bits": 1065353216, "native_scale": 1.0},
                   {"native_scale": None, "native_scale_bits": None, "native_scale_source": "unknown",
                    "native_scale_query_request": None}]
        for delta in changes:
            with self.subTest(delta=delta), self.assertRaises(self.hold):
                self.scan([first, self.query(observation_generation=2, **delta)])
        rescaled = self.query(native_scale_source="rescaled_event", native_scale_query_request=None)
        with self.assertRaises(self.hold):
            self.scan([rescaled, self.query(observation_generation=2, native_scale_generation=8)])
        with self.assertRaises(self.hold):
            self.scan([self.record(native_scale_generation=7),
                       self.record(observation_generation=2, native_scale_generation=8)])
        with self.assertRaises(self.hold):
            self.scan([self.query(window_open_generation=3),
                       self.query(observation_generation=2, window_open_generation=2)])

    def test_actual_readiness_requires_latest_complete_positive_factor(self):
        ready = self.selected["selected_actual_ready"]
        unknown = self.record()
        known = self.query(observation_generation=2, native_scale_generation=1)
        reopened = self.record(observation_generation=3, window_open_generation=2, native_scale_generation=1)
        actual = self.scan([unknown, known, reopened])
        adapter = [{"backend": "Gl"}]
        self.assertFalse(ready([], adapter, False))
        self.assertFalse(ready(actual[:1], adapter, False))
        self.assertTrue(ready(actual[:2], adapter, False))
        self.assertFalse(ready(actual[:2], adapter, True))
        self.assertFalse(ready(actual[:2], [], False))
        for _ in range(3):
            # The actual readiness expression may not choose the older known
            # factor when the latest real lifecycle is still unknown. This is
            # a pure guard oracle, NOT a simulated deadline/API completion.
            self.assertFalse(ready(actual, adapter, False))

    def test_actual_frame_key_keeps_lifecycle_query_generation_and_state(self):
        first = self.query()
        frame = self.frame(first, {"offset": 100, "read_time": 1})
        self.assertEqual(frame, self.frame(first, {"offset": 200, "read_time": 2, "unrelated_log": True}))
        for delta in [{"window_open_generation": 2}, {"native_scale_query_request": QUERY_B},
                      {"observation_generation": 2}]:
            with self.subTest(delta=delta):
                self.assertNotEqual(frame, self.frame(self.query(**delta), {}))
        middle = copy.deepcopy(first)
        middle["observation_generation"] = 2
        middle["state"]["history"]["rows"] = 400
        middle["state"]["inspector_open"] = True
        last = copy.deepcopy(first)
        last["observation_generation"] = 3
        self.assertNotEqual(frame, self.frame(middle, {}))
        self.assertNotEqual(frame, self.frame(last, {}))
        query_at_limit = self.query(native_scale_generation=MAX_U64)
        rescaled_at_limit = self.query(native_scale_generation=MAX_U64,
                                      native_scale_source="rescaled_event", native_scale_query_request=None)
        self.assertNotEqual(self.frame(query_at_limit, {}), self.frame(rescaled_at_limit, {}))


if __name__ == "__main__":
    unittest.main()
