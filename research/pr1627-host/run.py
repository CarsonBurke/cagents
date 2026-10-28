#!/usr/bin/env python3
"""One hosted, natural-User HOST qualification; no native UI or package claim."""
from pathlib import Path
import hashlib
import importlib.util
import json
import math
import os
import re
import signal
import subprocess
import sys
import time

HERE = Path(__file__).resolve().parent
REPO = HERE.parent.parent
OUTPUT = Path(os.environ["RUNNER_TEMP"]) / "pr1627-host-results"
TARGET = Path(os.environ["RUNNER_TEMP"]) / "pr1627-host-target"
TOTAL_SECONDS = 3000  # Absolute budget includes checkout/setup from first step.
CANCEL = []
RESULTS = []


def digest(path):
    return hashlib.sha256(Path(path).read_bytes()).hexdigest()


def git(*args, cwd=REPO):
    return subprocess.check_output(["git", "--no-optional-locks", *args], cwd=cwd, text=True).strip()


def put(path, value):
    temporary = path.with_suffix(path.suffix + ".tmp")
    temporary.write_text(json.dumps(value, indent=2) + "\n")
    temporary.replace(path)


def say(message):
    try:
        print(message, flush=True)
    except OSError:
        pass  # A closed console must not unwind owned command cleanup.


def summary(line):
    path = os.environ.get("GITHUB_STEP_SUMMARY")
    if path:
        with Path(path).open("a") as stream:
            stream.write(line + "\n")


def point(source, expected):
    assert git("rev-parse", "HEAD", cwd=source) == expected["head"]
    assert git("rev-parse", "HEAD^{tree}", cwd=source) == expected["tree"]
    assert not git("status", "--porcelain=v1", "--untracked-files=all", cwd=source)
    transport = (source / "src/chat/native_holder/transport.rs").read_bytes()
    marker = b"#[cfg(test)]\nmod tests {"
    assert transport.count(marker) == 1
    assert hashlib.sha256(transport.split(marker, 1)[1]).hexdigest() == MANIFEST["owning_tests_sha256"]
    for name, expected_hash in expected["product_files_sha256"].items():
        assert digest(source / name) == expected_hash
    assert digest(HERE / "observe-command-owned.py") == MANIFEST["observer_sha256"]
    assert digest(HERE / "kernel_contract.py") == MANIFEST["kernel_contract_sha256"]
    assert digest(__file__) == MANIFEST["harness_sha256"]


def terminate_ready(child, pidfd):
    # Only the exact unreaped observer child is signalled, through its pidfd.
    # Its SIGTERM handler must already own command cleanup before cancellation.
    if pidfd is None or child.poll() is not None:
        return
    try:
        os.waitid(os.P_PIDFD, pidfd, os.WEXITED | os.WNOHANG | os.WNOWAIT)
        fields = dict(line.split(":", 1) for line in Path(f"/proc/{child.pid}/status").read_text().splitlines() if ":" in line)
        if int(fields["SigCgt"].strip(), 16) & (1 << (signal.SIGTERM - 1)):
            signal.pidfd_send_signal(pidfd, signal.SIGTERM)
    except (FileNotFoundError, ProcessLookupError, ChildProcessError):
        pass


def hold_unproved(output):
    say("CLEANUP UNPROVED: no later phase or target reuse; hosted job must terminate.")
    try:
        put(OUTPUT / "CLEANUP-UNPROVED.json", {"phase": str(output), "target_reuse": False})
    except BaseException:
        pass
    while True:
        time.sleep(1)


def phase(name, source, pin, command, cap=2400):
    point(source, pin)
    remaining = math.floor(DEADLINE - time.monotonic())
    if CANCEL or remaining < 1:
        raise RuntimeError("cancelled or total research budget exhausted before next phase")
    timeout = min(cap, remaining)
    output = OUTPUT / name
    env = os.environ.copy()  # Preserve authority, HOME, scopes and no-start.
    env["CARGO_TARGET_DIR"] = str(TARGET)
    env["CARGO_BUILD_JOBS"] = "2"
    contract.HEAD, contract.TREE = pin["head"], pin["tree"]
    observer = [sys.executable, "-I", "-B", str(HERE / "observe-command-owned.py"),
                "--output", str(output), "--timeout", str(timeout), "--", *command]
    say(f"START {name}: source={pin['head']} residual={remaining}s timeout={timeout}s")
    child = subprocess.Popen(observer, cwd=source, env=env, start_new_session=True)
    pidfd = None
    error = None
    try:
        pidfd = os.pidfd_open(child.pid)
        put(OUTPUT / "RUNNING.json", {"phase": name, "source": pin, "command": command,
                                     "timeout_seconds": timeout, "remaining_seconds": remaining})
        next_heartbeat = time.monotonic()
        while child.poll() is None:
            if CANCEL or time.monotonic() >= DEADLINE:
                terminate_ready(child, pidfd)
            if time.monotonic() >= next_heartbeat:
                say(f"RUNNING {name}: elapsed_total={int(time.monotonic() - BEGIN)}s")
                next_heartbeat = time.monotonic() + 15
            time.sleep(0.1)
    except BaseException as caught:
        error = type(caught).__name__
    finally:
        # Evidence failure, cancellation, or budget expiry cannot bypass wait.
        # Cleanup can outlive the execution budget; it never permits reuse.
        while child.poll() is None:
            try:
                terminate_ready(child, pidfd)
                time.sleep(0.1)
            except BaseException:
                continue
        child.wait()
        if pidfd is not None:
            os.close(pidfd)
        try:
            closed, receipt = contract.valid_closed(output, child, command)
        except BaseException:
            closed, receipt = False, None
        if not closed:
            hold_unproved(output)
    point(source, pin)
    result = {"name": name, "source": pin, "actual_exit": receipt["actual_exit"],
              "command_exit": receipt["command_exit"], "closed": True,
              "wall_seconds": receipt["wall_seconds"], "command": command}
    RESULTS.append(result)
    put(OUTPUT / "PHASES.json", RESULTS)
    say(f"CLOSED {name}: actual_exit={result['actual_exit']} wall={result['wall_seconds']:.2f}s")
    summary(f"- {name}: actual exit {result['actual_exit']}, owned family closed, source `{pin['head']}`")
    if error or CANCEL:
        raise RuntimeError("supervisor error or cancellation after proven closure")
    return result, output


def compiled_lib(label, source, pin, features):
    command = ["cargo", "test", "--locked", "--no-default-features", "--features", features,
               "--lib", "--no-run", "--message-format=json"]
    result, output = phase(label, source, pin, command)
    assert result["actual_exit"] == 0, f"{label}: compilation failed; inspect raw logs"
    events = []
    for line in (output / "stdout.log").read_text().splitlines():
        try:
            events.append(json.loads(line))
        except ValueError:
            continue
    assert any(event.get("reason") == "build-finished" and event.get("success") is True for event in events)
    artifacts = [event for event in events if event.get("reason") == "compiler-artifact"
                 and event.get("target", {}).get("name") == "cagents"
                 and event.get("target", {}).get("kind") == ["lib"]
                 and event.get("profile", {}).get("test") is True and event.get("executable")]
    assert len(artifacts) == 1, "must select exactly the owning lib test artifact from this command"
    artifact = artifacts[0]
    selected = set(artifact["features"])
    assert "host" in selected and not selected.intersection({"ui", "integrated-browser", "latency-harness", "full"})
    executable = Path(artifact["executable"]).resolve(strict=True)
    assert executable.is_relative_to(TARGET.resolve(strict=True)) and executable.is_file()
    record = {"source": pin, "features": sorted(selected), "executable": str(executable),
              "sha256": digest(executable), "cargo_phase": label}
    put(output / "SELECTED-LIB-ARTIFACT.json", record)
    return record


def test(label, source, pin, artifact, selector, refusal=None):
    assert digest(artifact["executable"]) == artifact["sha256"]
    command = [artifact["executable"], selector, "--exact", "--nocapture", "--test-threads=1", "--color=never"]
    result, output = phase(label, source, pin, command, cap=180)
    text = (output / "stdout.log").read_text() + "\n" + (output / "stderr.log").read_text()
    summaries = re.findall(r"^test result: (ok|FAILED)\. (\d+) passed; (\d+) failed;", text, re.M)
    named = f"test {selector} ..." in text
    if refusal is None:
        accepted = result["actual_exit"] == 0 and named and summaries and summaries[-1] == ("ok", "1", "0")
    else:
        accepted = (result["actual_exit"] == result["command_exit"] == 101 and named and summaries
                    and summaries[-1] == ("FAILED", "0", "1")
                    and any(marker in text for marker in refusal))
    result["selector"] = selector
    result["expected_control_failure"] = refusal is not None
    result["qualification_exit"] = 0 if accepted else 1
    put(output / "TEST-QUALIFICATION.json", result)
    put(OUTPUT / "PHASES.json", RESULTS)
    summary(f"  - Exact selector `{selector}`: qualification exit {result['qualification_exit']}")
    return bool(accepted)


def checkout(label):
    pin = MANIFEST["sources"][label]
    source = Path(os.environ["RUNNER_TEMP"]) / ("pr1627-source-" + label)
    assert not source.exists()
    subprocess.run(["git", "worktree", "add", "--detach", str(source), pin["head"]], cwd=REPO, check=True)
    point(source, pin)
    return source, pin


def main():
    global MANIFEST, contract, BEGIN, DEADLINE
    for sig in (signal.SIGHUP, signal.SIGINT, signal.SIGTERM):
        signal.signal(sig, lambda number, _frame: CANCEL.append(number))
    assert os.environ.get("GITHUB_ACTIONS") == "true", "hosted research only; no local launch admission"
    assert os.environ.get("CARGO_BUILD_JOBS") == "2"
    assert os.environ.get("RUSTUP_TOOLCHAIN") == "1.93.0"
    assert "CAGENTS_AGENT_AUTHORITY" not in os.environ, "natural-User hosted caller required; never remove inherited authority"
    for name in ("CAGENTS_NATIVE_UI_FIXTURE", "CAGENTS_LATENCY_OUTPUT", "CAGENTS_TEST_RETAINED_ACTOR_HELPER",
                 "CAGENTS_PR_FIXTURE_CASE", "CAGENTS_NATIVE_DIRECTORY_EXIT_TEST"):
        assert name not in os.environ, "unexpected ambient fixture/helper selection"
    MANIFEST = json.loads((HERE / "manifest.json").read_text())
    for name, key in [("observe-command-owned.py", "observer_sha256"),
                      ("kernel_contract.py", "kernel_contract_sha256"), ("run.py", "harness_sha256")]:
        assert digest(HERE / name) == MANIFEST[key]
    assert digest(REPO / ".github/actions/setup-linux-build/action.yml") == MANIFEST["setup_sha256"]
    assert git("rev-parse", "HEAD") == os.environ["GITHUB_SHA"]
    allowed = {".github/workflows/pr1627-host-qualification.yml", *[f"research/pr1627-host/{name}" for name in
               ("run.py", "manifest.json", "kernel_contract.py", "observe-command-owned.py", "README.md")]}
    delta = git("diff", "--name-status", MANIFEST["sources"]["after"]["head"], "HEAD").splitlines()
    assert delta and all(line.startswith("A\t") and line[2:] in allowed for line in delta)
    assert {line[2:] for line in delta} == allowed, "research tree may only add the six reviewed research files"
    for label, pin in MANIFEST["sources"].items():
        assert git("rev-parse", pin["head"] + "^{tree}") == pin["tree"]
        subprocess.run(["git", "merge-base", "--is-ancestor", pin["head"], "HEAD"], cwd=REPO, check=True)
    spec = importlib.util.spec_from_file_location("pr1627_kernel_contract", HERE / "kernel_contract.py")
    contract = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(contract)
    OUTPUT.mkdir(exist_ok=False)
    BEGIN = float(os.environ["PR1627_JOB_START"])
    DEADLINE = float(os.environ["PR1627_JOB_DEADLINE"])
    now = time.monotonic()
    assert math.isfinite(BEGIN) and 0 <= BEGIN <= now
    assert math.isfinite(DEADLINE) and abs(DEADLINE - BEGIN - TOTAL_SECONDS) < 0.001
    budget_record = json.loads((Path(os.environ["RUNNER_TEMP"]) / "pr1627-job-budget.json").read_text())
    assert abs(budget_record["earliest_step_monotonic"] - BEGIN) < 0.001
    assert abs(budget_record["deadline_monotonic"] - DEADLINE) < 0.001
    assert budget_record["total_budget_seconds"] == TOTAL_SECONDS
    put(OUTPUT / "RESEARCH-START.json", {"research_head": git("rev-parse", "HEAD"),
        "research_tree": git("rev-parse", "HEAD^{tree}"), "manifest_sha256": digest(HERE / "manifest.json"),
        "absolute_budget_seconds": TOTAL_SECONDS, "setup_and_preflight_elapsed_seconds": now - BEGIN,
        "remaining_seconds_at_harness_start": max(0, DEADLINE - now),
        "authority_present": "CAGENTS_AGENT_AUTHORITY" in os.environ,
        "authority": "naturally absent; no value read, removed, or fabricated", "claims": "HOST compiler and six owning parent tests plus one same-test privacy negative only"})
    summary("PR1627 HOST qualification. Actual native error-display/provider UI qualification remains pending.\n")
    passed = True
    after, pin = checkout("after")
    checked, _ = phase("01-after-host-bin-check", after, pin,
        ["cargo", "check", "--locked", "--no-default-features", "--features", "host", "--bin", "cagentsd"])
    assert checked["actual_exit"] == 0
    artifact = compiled_lib("02-after-host-lib-compile", after, pin, "host")
    for index, selector in enumerate(MANIFEST["positive_selectors"], 3):
        passed = test(f"{index:02}-after-owning-{index-2}", after, pin, artifact, selector)
        if not passed:
            break
    # Never qualify a negative if the final-source owning positive did not pass.
    if passed:
        control, control_pin = checkout("privacy_control")
        negative = compiled_lib("09-privacy-control-host-lib-compile", control, control_pin, "host")
        passed = test("10-privacy-same-test-negative", control, control_pin, negative,
                      MANIFEST["privacy_selector"], refusal=MANIFEST["privacy_control_refusals"])
    put(OUTPUT / "RESULT.json", {"qualification_exit": 0 if passed else 1, "phases": RESULTS,
        "limits": "No native window/error-display/copy/Close/provider service/auth or matched-package qualification. Issue1584 direct-stdio/full-log scope remains separate."})
    summary("\nOverall HOST research qualification: " + ("PASS" if passed else "FAIL"))
    return 0 if passed else 1


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except Exception as error:
        say("Research failed closed: " + type(error).__name__)
        if OUTPUT.exists():
            put(OUTPUT / "HARNESS-ERROR.json", {"error_type": type(error).__name__, "phases": RESULTS})
        raise
