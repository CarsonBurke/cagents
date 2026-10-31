#!/usr/bin/env python3
"""One PR1310 arm in its ORIGINAL future admitted parent's process family.

This leaf implements the real worker, not the still-missing parent controller.
Its arguments identify immutable inputs and an existing absolute deadline; they
are not User authorization, a C allocation, or evidence of a successful build.
Importing this module performs no native import, acquisition, or launch.
"""

import argparse
import hashlib
import importlib
import importlib.util
import json
import math
import os
from pathlib import Path
import signal
import stat
import sys
import time


LIBRARY_HEAD = "b756e6f9165f6bf164b8acad1c77cdab34d3fd07"
LIBRARY_TREE = "825fdfda1d7f15d928683449259d86ced8493d23"
LIBRARY_FILES = {
    "native_owner.py": "e2e4a520087bf95c902646b4dbe6b69531944bb3cfad5fc8ddc1d3f77aab9fd8",
    "history_driver.py": "0bd819fce05fc0559da8b874a190e666f5cc19ce768af4e23ee21690deaa27ce",
    "observations.py": "bb97545b94c76d7ea85e404109241e49b3b985f97ba95aad52029cca5e22d4d2",
}
CUSTODY_SHA = "d9eeec3f2e228e4844695de096f85ecf465381ebd0a2c7a85e964b7103ebe0d9"
PRIVATE_KEYS = ("XDG_CONFIG_HOME", "XDG_DATA_HOME", "XDG_STATE_HOME",
                "XDG_CACHE_HOME", "XDG_RUNTIME_DIR", "TMPDIR")
ENDPOINT_KEYS = ("DISPLAY", "XAUTHORITY", "DBUS_SESSION_BUS_ADDRESS", "AT_SPI_BUS_ADDRESS")
MAX_PACKET = 4 * 1024 * 1024
MAX_REPORT = 64 * 1024 * 1024
STOP_SIGNALS = []


def require(value, reason):
    if not value:
        raise RuntimeError(reason)


def cancelled():
    return bool(STOP_SIGNALS)


def stop(signum, _frame):
    if len(STOP_SIGNALS) < 64:
        STOP_SIGNALS.append(signum)


def checkpoint(deadline):
    require(not cancelled(), "original parent/worker cancellation observed")
    remaining = deadline - time.monotonic()
    require(remaining > 0, "original whole deadline exhausted; no renewed budget")
    return remaining


def frozen_bytes(spelling, expected, maximum):
    """Source/packet preflight only; not a build or effective-config producer."""
    path = Path(spelling)
    require(path.is_absolute() and str(path) == spelling
            and path.resolve(strict=True) == path, "input path is absent or aliased")
    before = path.lstat()
    require(stat.S_ISREG(before.st_mode) and before.st_size <= maximum,
            "input is not a bounded ordinary file")
    data = path.read_bytes()
    after = path.lstat()
    identity = lambda value: (value.st_dev, value.st_ino, value.st_uid, value.st_mode,
                              value.st_nlink, value.st_size, value.st_mtime_ns, value.st_ctime_ns)
    require(identity(before) == identity(after) and len(data) == before.st_size
            and hashlib.sha256(data).hexdigest() == expected, "immutable input changed")
    return data


def unique_json(data):
    def unique(pairs):
        value = {}
        for key, item in pairs:
            require(key not in value, "duplicate input JSON key")
            value[key] = item
        return value
    def nonfinite(_value):
        raise RuntimeError("nonfinite input JSON")
    return json.loads(data.decode("utf-8", errors="strict"), object_pairs_hook=unique,
                      parse_constant=nonfinite)


def source_module(name, path, expected, deadline):
    """Load only the exact immutable source, under the future original parent."""
    require(name not in sys.modules, "module already initialized before source binding")
    checkpoint(deadline)
    data = frozen_bytes(str(path), expected, 1024 * 1024)
    specification = importlib.util.spec_from_file_location(name, path)
    require(specification is not None and specification.loader is not None,
            "source module loader unavailable")
    module = importlib.util.module_from_spec(specification)
    sys.modules[name] = module
    checkpoint(deadline)
    # Compile the bytes just checked, rather than reopening the source through
    # a loader or creating a pycache beside immutable qualification libraries.
    exec(compile(data, str(path), "exec"), module.__dict__)
    checkpoint(deadline)
    frozen_bytes(str(path), expected, 1024 * 1024)
    return module


def emit(value):
    """Original parent must retain this pipe/file and its ORIGINAL worker wait."""
    payload = (json.dumps(value, sort_keys=True, allow_nan=False) + "\n").encode()
    require(len(payload) <= MAX_REPORT, "worker report exceeds original byte cap")
    sys.stdout.buffer.write(payload)
    sys.stdout.buffer.flush()


def held(owner, error):
    """No detach, speculative cleanup, PID lookup, or new supervisor on unknown."""
    # The original parent still owns this worker Popen/pidfd and C. Its source
    # controller is a missing prerequisite, not supplied by this waiting leaf.
    try:
        emit({"record": "pr1310-arm-retained-hold-v1", "worker_pid": os.getpid(),
              "error_type": type(error).__name__, "error": str(error)[:4096],
              "first_failure": None if owner is None else owner.first_failure,
              "events": [] if owner is None else owner.events,
              "physical_evidence": {} if owner is None else owner.physical_evidence,
              "logical_allocation": "HELD_EXTERNAL_ROOT", "native": "UNQUALIFIED",
              "continued_work": False})
    finally:
        # An original deadline expiring does not authorize abandoning children
        # or retrying an ambiguous close. Signals latch; no task is polled here.
        while True:
            signal.pause()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--packet", required=True)
    parser.add_argument("--packet-sha256", required=True)
    parser.add_argument("--worker-sha256", required=True)
    parser.add_argument("--whole-deadline", required=True, type=float)
    arguments = parser.parse_args()
    require(sys.flags.isolated == 1 and sys.dont_write_bytecode,
            "the original parent must select its pinned physical Python with -I -B")
    deadline = arguments.whole_deadline
    require(math.isfinite(deadline) and 0 < deadline - time.monotonic() <= 2400,
            "use the original parent's unchanged absolute 2400-second cap")
    signal.signal(signal.SIGINT, stop)
    signal.signal(signal.SIGTERM, stop)
    require(signal.getsignal(signal.SIGCHLD) == signal.SIG_DFL,
            "inherited child-reaping policy refused unchanged")
    worker_path = Path(__file__).resolve(strict=True)
    frozen_bytes(str(worker_path), arguments.worker_sha256, 1024 * 1024)
    packet = unique_json(frozen_bytes(arguments.packet, arguments.packet_sha256, MAX_PACKET))
    require(set(packet) == {"schema", "arm", "root", "library_directory", "custody_path",
                            "sources", "tools", "native_import_inputs"}
            and packet["schema"] == "pr1310-original-arm-inputs-v1",
            "unsupported actual-arm input fields")
    arm = packet["arm"]
    require(arm in ("before", "after") and set(packet["sources"]) == {arm},
            "one immutable arm per original worker; no target repinning")
    library = Path(packet["library_directory"])
    require(library.is_absolute() and library.resolve(strict=True) == library,
            "library source directory is absent or aliased")
    require(packet["sources"][arm]["driver"] == {
        "path": str(library / "history_driver.py"),
        "sha256": LIBRARY_FILES["history_driver.py"]}, "different original driver source")
    for name, expected in LIBRARY_FILES.items():
        checkpoint(deadline)
        frozen_bytes(str(library / name), expected, 1024 * 1024)
    # No discovery, fallback module path, native import, or authority rewrite.
    custody_module = source_module("pr1310_original_custody", Path(packet["custody_path"]),
                                   CUSTODY_SHA, deadline)
    observations = source_module("observations", library / "observations.py",
                                 LIBRARY_FILES["observations.py"], deadline)
    native_owner = source_module("pr1310_native_owner", library / "native_owner.py",
                                 LIBRARY_FILES["native_owner.py"], deadline)
    driver_module = source_module("pr1310_history_driver", library / "history_driver.py",
                                  LIBRARY_FILES["history_driver.py"], deadline)
    checkpoint(deadline)
    owner = None
    generations = []
    flow_completed = False
    try:
        custody = custody_module.Custody(cancelled)
        checkpoint(deadline)
        owner = native_owner.NativeHistoryOwner(
            custody, cancelled, phase_deadline=deadline, whole_deadline=deadline,
            root=packet["root"], sources=packet["sources"], tools=packet["tools"],
            entry_function=main, entry_source=(str(worker_path), arguments.worker_sha256))
        # The ORIGINAL owner is retained before any desktop partial acquisition.
        overlay = owner.prepare_private_desktop()
        require(set(overlay) == set(PRIVATE_KEYS + ENDPOINT_KEYS),
                "private endpoint return contract changed")
        original_environment = os.environ.copy()
        checkpoint(deadline)
        os.environ.update(overlay)
        require(dict(os.environ) == {**original_environment, **overlay},
                "private adoption changed unrelated original environment")
        # Actual installed-module input closure is supplied by the future
        # parent/admission, not discovered here or inferred from these hashes.
        inputs = packet["native_import_inputs"]
        require(isinstance(inputs, list) and 1 <= len(inputs) <= 4096,
                "actual pinned native import inputs absent")
        require(len({item["path"] for item in inputs}) == len(inputs),
                "duplicate native import input")
        for item in inputs:
            require(set(item) == {"path", "sha256"}, "native input fields changed")
            owner._pin_input(Path(item["path"]), item["sha256"], 1024 * 1024 * 1024)
        require(not any(name == "pyatspi" or name.startswith("pyatspi.")
                        or name == "gi" or name.startswith("gi.")
                        or name == "PIL" or name.startswith("PIL.") for name in sys.modules),
                "native modules were initialized before owned endpoint adoption")
        checkpoint(deadline)
        atspi = importlib.import_module("pyatspi")
        checkpoint(deadline)
        image = importlib.import_module("PIL.Image")
        checkpoint(deadline)
        require(dict(os.environ) == {**original_environment, **overlay},
                "native initialization changed inherited worker environment")
        spec = packet["sources"][arm]
        manifest = owner._open_input(Path(spec["manifest"]), spec["manifest_sha256"])
        for dataset in ("small", "large", "unborn"):
            checkpoint(deadline)
            index = (None if dataset == "unborn" else owner._open_input(
                Path(spec["indexes"][dataset]), spec["index_sha256"][dataset]))
            fixture = observations.Fixture.from_owned_bytes(manifest, index, dataset)
            driver = driver_module.HistoryDriver(owner, atspi, image, arm, fixture)
            driver.run_current_dataset()
            owner.close_history_root(driver.binding)
            generations.append(owner.history_application_outcome(driver.binding))
        flow_completed = True
    except BaseException as error:
        if owner is None:
            raise  # No desktop/child owner was returned or entered.
        owner.record_history_failure(error)
    if owner is None:
        raise RuntimeError("original owner absent")
    try:
        for binding in owner.bindings:
            if not binding.closed:
                owner.close_history_root(binding)
                generations.append(owner.history_application_outcome(binding))
        settlement = owner.settle_history_services()
        # Final report is a separate original-worker writer. Its return and
        # process terminal/pidfd death must be observed by the missing parent;
        # the owner's earlier local no-more-work marker does not include it.
        emit({"record": "pr1310-original-arm-raw-v1", "arm": arm,
              "worker_pid": os.getpid(), "library": [LIBRARY_HEAD, LIBRARY_TREE],
              "packet_sha256": arguments.packet_sha256,
              "worker_sha256": arguments.worker_sha256,
              "original_whole_deadline": deadline, "stop_signals": STOP_SIGNALS,
              "flow_method_returned": flow_completed, "generations": generations,
              "settlement": settlement, "events": owner.events,
              "logical_allocation": "HELD_EXTERNAL_ROOT",
              "application": "UNKNOWN_PENDING_EXTERNAL_APPLICATION_REVIEW",
              "native": "UNQUALIFIED", "C_release": "NOT_PERFORMED_BY_WORKER"})
        return 0 if flow_completed and owner.first_failure is None else 1
    except BaseException as error:
        owner.record_history_failure(error)
        held(owner, error)


if __name__ == "__main__":
    raise SystemExit(main())
