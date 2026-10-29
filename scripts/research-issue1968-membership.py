#!/usr/bin/env python3
"""NEVERMERGE: exact candidate qualification; no full-HOST or speedup claim.

The workflow invokes these stages sequentially on one hosted runner. Commands
inherit the natural User environment unchanged. The original scale fixture is
the sole owner of its subprocess family and namespace cleanup.
"""

import hashlib
import json
import os
from pathlib import Path
import re
import resource
import shutil
import subprocess
import sys
import time


PRODUCT_SHA = "c96008f4f7e7e6d8d96a5d34d0dcc388bfc970ab"
PRODUCT_TREE = "e28a2f909181a92332e8ee1183ef21f4057d156b"
SETUP_SHA = "c727f886628897c1749290213e9d65037253095a"
SETUP_TREE = "6d4f36e2b4dd48fc9744305d34c78469384edcc5"
SETUP_ACTION = ".github/actions/setup-linux-build/action.yml"
SETUP_HASH = "82a5284e0640b3c6648e3b9b3d6e0e0b323ada03eb087cbf95b9b63ca3d0c052"
PRODUCT_HASHES = {
    "Cargo.toml": "8cf513d01300436076de1092e3c45cbbadc6c3c4b49286b65f882d7a3da92e48",
    "Cargo.lock": "cbcb8b31eb6b00cbafe0a087145e2e6274ce5775413827b4f35295f0b93c383e",
    "src/repository.rs": "a5f586b27a589cb81b18b70fe371c84431b6f16e13b775ecee7ea8a357cdaf32",
    "tests/repository_scale_contract.rs": "fab265f52912cd89aded0b703c8e2d020865ac0f44e8b43a4c4cc1a1f2d176f6",
    "tests/support/repository_scale.rs": "5cc407ebe3e76f6873b04ebf7de5992a2f22de6f32ed84cba8c65f0d530c429d",
}
CONTROL_FILES = [
    ".github/workflows/research-issue1968-membership.yml",
    "scripts/research-issue1968-membership.py",
    "docs/research-issue1968-membership.md",
]
LARGE_NAMES = [
    "repository::tests::large_membership_queries_use_one_census_and_keep_exact_selected_paths",
    "repository::tests::large_membership_census_output_limit_retries_every_selected_batch",
    "repository::tests::large_membership_census_errors_never_fall_back_or_start_a_mutation",
    "repository::tests::git_contracts::large_membership_census_preserves_real_git_entry_kinds_and_byte_paths",
]
EXACT_PARENTS = [
    "repository::tests::git_contracts::unstage_preserves_worktree_head_and_unselected_index_entries",
    "repository::tests::git_contracts::unborn_unstage_does_not_require_worktree_to_match_staged_content",
    "repository::tests::git_contracts::unstage_index_lock_refusal_preserves_staged_content",
    "repository::tests::a_rename_source_the_index_has_lost_is_not_sent_as_a_pathspec",
    "repository::tests::unstage_on_an_unborn_head_with_no_indexed_paths_runs_no_mutation",
    "repository::tests::pathspec_commands_place_literal_pathspecs_before_the_repository",
]
SCALE_NAME = "large_literal_pathspec_mutations_roundtrip_without_argv_limits"
PHASE_NAMES = [
    "child entry", "contract", "repository init", "literal generation",
    "literal baseline stage", "literal baseline commit", "literal modification",
    "literal modified stage", "literal unstage", "literal discard", "literal verify",
    "40k generation", "40k pathspec size verify", "40k initial stage",
    "40k initial unstage", "40k untracked discard", "40k untracked verify",
    "40k baseline generation", "40k baseline stage", "40k baseline commit",
    "40k modification", "40k modified stage", "40k tracked unstage",
    "40k tracked discard", "40k tracked verify", "settlement",
]
WORKSPACE = Path(os.environ["GITHUB_WORKSPACE"])
PRODUCT = WORKSPACE / "product"
CONTROL = WORKSPACE / "research"
SETUP = WORKSPACE / "setup-main"
LOGS = Path(os.environ["RUNNER_TEMP"]) / "issue1968-membership-logs"


def write_json(name, value):
    (LOGS / name).write_text(json.dumps(value, indent=2, sort_keys=True) + "\n")


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def natural_context(label):
    # Presence only: never inspect or print authority, provider or test values.
    names = sorted(name for name in os.environ if name.startswith("CAGENTS_"))
    context = {
        "present_cagents_names": names,
        "present_cagents_test_names": [name for name in names if name.startswith("CAGENTS_TEST_")],
        "required_absent": {
            name: name in os.environ for name in [
                "CAGENTS_AGENT_AUTHORITY", "CAGENTS_TEST_RETAINED_ACTOR_HELPER",
                "CAGENTS_LARGE_PATHSPEC_CONTRACT", "CAGENTS_LARGE_PATHSPEC_ROOT",
            ]
        },
    }
    write_json(label + "-context.json", context)
    if names:
        raise RuntimeError("Natural User context refused; present names: " + ", ".join(names))
    if os.environ.get("CARGO_BUILD_JOBS") != "2":
        raise RuntimeError("Expected exactly CARGO_BUILD_JOBS=2")
    if os.environ.get("RUSTUP_TOOLCHAIN") != "1.93.0":
        raise RuntimeError("Expected the pinned Rust 1.93.0 toolchain")


def git(path, *args):
    return subprocess.check_output(["git", "-C", str(path), *args], text=True).strip()


def snapshot(label):
    result = {"repositories": {}, "product_sha256": {}, "control_sha256": {}}
    # Write observations before rejecting an unexpected source or dirty tree.
    for name, path in [("product", PRODUCT), ("research", CONTROL), ("setup-main", SETUP)]:
        if path.is_dir():
            try:
                result["repositories"][name] = {
                    "head": git(path, "rev-parse", "HEAD"),
                    "tree": git(path, "rev-parse", "HEAD^{tree}"),
                    "status": git(path, "status", "--porcelain", "--untracked-files=all"),
                }
            except subprocess.CalledProcessError as error:
                result["repositories"][name] = {"observation_error": str(error)}
        else:
            result["repositories"][name] = {"unavailable": True}
    for name in PRODUCT_HASHES:
        if (PRODUCT / name).is_file():
            result["product_sha256"][name] = digest(PRODUCT / name)
    for name in CONTROL_FILES:
        if (CONTROL / name).is_file():
            result["control_sha256"][name] = digest(CONTROL / name)
    if (SETUP / SETUP_ACTION).is_file():
        result["setup_action_sha256"] = digest(SETUP / SETUP_ACTION)
    write_json(label + ".json", result)
    expected = {
        "product": (PRODUCT_SHA, PRODUCT_TREE),
        "research": (os.environ["EXPECTED_CONTROL_SHA"], None),
        "setup-main": (SETUP_SHA, SETUP_TREE),
    }
    for name, (head, tree) in expected.items():
        observed = result["repositories"][name]
        if observed.get("head") != head or (tree and observed.get("tree") != tree) or observed.get("status"):
            raise RuntimeError("Unexpected source or dirty checkout: " + name)
    if result["product_sha256"] != PRODUCT_HASHES:
        raise RuntimeError("Literal product or frozen harness hash mismatch")
    if result.get("setup_action_sha256") != SETUP_HASH:
        raise RuntimeError("Pinned setup action hash mismatch")
    if len(result["control_sha256"]) != len(CONTROL_FILES):
        raise RuntimeError("Missing controller source")
    before = LOGS / "source-before.json"
    if label != "source-before" and before.exists():
        if result != json.loads(before.read_text()):
            raise RuntimeError("Source observations differ from the pre-build snapshot")
    return result


def command(label, argv):
    natural_context(label)
    path = LOGS / (label + ".txt")
    record = {
        "label": label, "argv": argv, "cwd": str(PRODUCT),
        "started_unix_seconds": time.time(), "status": "running",
        "environment_policy": "Inherited unchanged; no authority removal or substitution",
    }
    write_json(label + "-command.json", record)
    before = resource.getrusage(resource.RUSAGE_CHILDREN)
    started = time.monotonic()
    print("Starting " + label + ": " + json.dumps(argv), flush=True)
    # No shell, outer timeout, signal, cleanup trap or environment rewriting.
    with path.open("xb") as output:
        result = subprocess.run(argv, cwd=PRODUCT, stdout=output, stderr=subprocess.STDOUT)
    after = resource.getrusage(resource.RUSAGE_CHILDREN)
    record.update({
        "status": "finished", "original_exit": result.returncode,
        "finished_unix_seconds": time.time(), "wall_seconds": time.monotonic() - started,
        "child_user_cpu_seconds": after.ru_utime - before.ru_utime,
        "child_system_cpu_seconds": after.ru_stime - before.ru_stime,
        "children_peak_rss_kib_cumulative": after.ru_maxrss,
        "rss_scope": "Cumulative waited-child peak for this controller stage; not per-command RSS",
        "log_sha256": digest(path),
    })
    (LOGS / (label + ".exit")).write_text(str(result.returncode) + "\n")
    write_json(label + "-command.json", record)
    output = path.read_text(errors="replace")
    print(output[-10000:], flush=True)
    if result.returncode:
        raise SystemExit(result.returncode if result.returncode > 0 else 128 - result.returncode)
    return output


def test(label, base, names, exact=False, scale=False):
    options = ["--exact"] if exact else []
    listed = command(label + "-list", base + ["--", *options, "--list", "--color=never"])
    selected = re.findall(r"^(.+): test$", listed, re.M)
    write_json(label + "-selection.json", {
        "expected_count": len(names), "expected_names": sorted(names), "observed_names": sorted(selected),
    })
    if not names or sorted(selected) != sorted(names):
        raise RuntimeError("Wrong or empty libtest selection: " + label)
    output = command(label, base + ["--", *options, "--show-output", "--test-threads=1", "--color=never"])
    summaries = re.findall(
        r"^test result: ok\. (\d+) passed; (\d+) failed; (\d+) ignored; (\d+) measured; (\d+) filtered out;",
        output, re.M,
    )
    if not summaries or tuple(map(int, summaries[-1][:4])) != (len(names), 0, 0, 0):
        raise RuntimeError("Missing exact nonzero passing outer libtest summary: " + label)
    for name in names:
        if not re.search(r"^test " + re.escape(name) + r" \.\.\. ok$", output, re.M):
            raise RuntimeError("Missing passing named test: " + name)
    if scale:
        phases = {}
        for name in PHASE_NAMES:
            matches = re.findall(r"^pathspec phase=" + re.escape(name) + r" end elapsed_ms=(\d+) result=finished$", output, re.M)
            if len(matches) != 1:
                raise RuntimeError("Missing completed original scale phase: " + name)
            phases[name] = int(matches[0])
        write_json("scale-phases.json", {"milliseconds": phases, "comparison": "Candidate only; no same-hardware baseline or speedup claim"})
    write_json(label + "-qualification.json", {"selected": names, "count": len(names), "result": "passed"})


def boundary_graph(label, feature=None):
    args = ["cargo", "tree", "--locked", "--no-default-features"]
    if feature:
        args += ["--features", feature]
    output = command(label + "-graph", args + ["--edges", "normal,build", "--prefix", "none", "--format", "{p}"])
    names = sorted(set(re.findall(r"^([^ ]+) v", output, re.M)))
    forbidden = [name for name in names if (
        (label == "ui" and "ghostty" in name.lower()) or
        (label == "host" and any(part in name.lower() for part in ["libcosmic", "servo", "webrender", "mozjs", "spidermonkey"]))
    )]
    write_json(label + "-graph-qualification.json", {"resolved_names": names, "forbidden_names": forbidden})
    if not names or forbidden:
        raise RuntimeError("Invalid isolated dependency graph: " + label)


def prepare():
    natural_context("prepare")
    snapshot("source-before")
    for group, root, names in [
        ("product", PRODUCT, PRODUCT_HASHES), ("research", CONTROL, CONTROL_FILES),
        ("setup-main", SETUP, [SETUP_ACTION]),
    ]:
        for name in names:
            destination = LOGS / "source" / group / name
            destination.parent.mkdir(parents=True, exist_ok=True)
            shutil.copyfile(root / name, destination)
    write_json("scope.json", {
        "candidate": PRODUCT_SHA, "tree": PRODUCT_TREE, "never_merge": True,
        "claim": "Narrow candidate repository qualification and isolated boundary checks only",
        "not_qualified": ["full HOST all-targets", "native UI flow", "integrated browser", "same-hardware before/after performance"],
        "original_scale": {"outer_tests": 1, "private_child_tests": 1, "child_timeout_seconds": 240, "output_limit_bytes_per_stream": 1048576, "file_count": 40000, "stdin_assertion_bytes_greater_than": 2097152},
        "cleanup": "Unchanged fixture owner only; controller never removes or signals its namespace or process family",
        "helper": "No separate cagentsd helper needed by the selected repository tests",
    })


def finalize():
    failures = []
    try:
        natural_context("final")
    except Exception as error:
        failures.append(str(error))
    try:
        snapshot("source-after-always")
    except Exception as error:
        failures.append(str(error))
    finally:
        expected = ["tools-rustfmt-install", "tools-rustc", "tools-cargo", "tools-rustfmt", "tools-zig", "tools-git", "tools-kernel", "tools-cpu", "tools-memory", "format", "host-graph", "host-check", "large-membership-list", "large-membership", "ghost-list", "ghost", "small-membership-list", "small-membership"]
        for index in range(len(EXACT_PARENTS)):
            expected += [f"parent-{index + 1}-list", f"parent-{index + 1}"]
        expected += ["scale-list", "scale", "cli-graph", "cli-check", "ui-graph", "ui-check"]
        records = {}
        for label in expected:
            path = LOGS / (label + "-command.json")
            records[label] = json.loads(path.read_text()) if path.exists() else {"status": "unrun"}
        passed = all(record.get("original_exit") == 0 and record.get("status") == "finished" for record in records.values())
        required_receipts = ["large-membership", "ghost", "small-membership", "scale"] + [f"parent-{index + 1}" for index in range(len(EXACT_PARENTS))]
        required_receipts += ["host-graph", "cli-graph", "ui-graph"]
        qualified = all((LOGS / (label + "-qualification.json")).is_file() for label in required_receipts)
        write_json("final-status.json", {"source_errors": failures, "commands": records, "narrow_qualification_complete": passed and qualified and not failures, "full_host_all_targets": "UNRUN", "speedup": "UNMEASURED"})
        inventory = []
        for path in sorted(LOGS.rglob("*")):
            if path.is_file() and path.name != "artifact-sha256.txt":
                inventory.append(digest(path) + "  " + str(path.relative_to(LOGS)))
        (LOGS / "artifact-sha256.txt").write_text("\n".join(inventory) + "\n")
    if failures:
        raise RuntimeError("; ".join(failures))
    if not passed or not qualified:
        raise RuntimeError("Narrow qualification incomplete; original failure and unrun commands retained")


def main(stage):
    if stage == "prepare":
        prepare()
    elif stage == "tools":
        for name, args in [
            ("rustfmt-install", ["rustup", "component", "add", "--toolchain", "1.93.0", "rustfmt"]),
            ("rustc", ["rustc", "-vV"]), ("cargo", ["cargo", "--version"]),
            ("rustfmt", ["rustfmt", "--version"]), ("zig", ["zig", "version"]),
            ("git", ["git", "--version"]), ("kernel", ["uname", "-a"]),
            ("cpu", ["lscpu"]), ("memory", ["free", "-b"]),
        ]:
            command("tools-" + name, args)
    elif stage == "host":
        command("format", ["scripts/check-format"])
        boundary_graph("host", "host")
        command("host-check", ["cargo", "check", "--locked", "--no-default-features", "--features", "host", "--bin", "cagentsd"])
    elif stage == "parents":
        base = ["cargo", "test", "--locked", "--no-default-features", "--features", "host", "--lib"]
        test("large-membership", base + ["large_membership"], LARGE_NAMES)
        test("ghost", base + ["large_unstage_with_a_ghost"], ["repository::tests::large_unstage_with_a_ghost_refuses_the_whole_selection"])
        test("small-membership", base + ["small_membership"], ["repository::tests::git_contracts::small_membership_queries_do_not_enumerate_large_index_or_head_tree"])
        for index, name in enumerate(EXACT_PARENTS):
            test(f"parent-{index + 1}", base + [name], [name], exact=True)
    elif stage == "scale":
        test("scale", ["cargo", "test", "--locked", "--no-default-features", "--features", "host", "--test", "repository_scale_contract", SCALE_NAME], [SCALE_NAME], exact=True, scale=True)
    elif stage == "cli":
        boundary_graph("cli")
        command("cli-check", ["cargo", "check", "--locked", "--no-default-features", "--bin", "cagents"])
    elif stage == "ui":
        boundary_graph("ui", "ui")
        command("ui-check", ["cargo", "check", "--locked", "--no-default-features", "--features", "ui", "--bin", "cagents-ui"])
    elif stage == "finalize":
        finalize()
    else:
        raise SystemExit("Unknown qualification stage: " + stage)


if __name__ == "__main__":
    stage = sys.argv[1]
    try:
        main(stage)
    except BaseException as error:
        write_json(stage + "-stage-result.json", {"completed": False, "error_type": type(error).__name__, "error": str(error)})
        raise
    else:
        write_json(stage + "-stage-result.json", {"completed": True})
