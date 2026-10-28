#!/usr/bin/env python3
"""Temporary exact-product hosted qualifier; never run from a delegated session.

The archived control bundle stays outside the disposable product checkout. Only
existing product tests execute; the observer's cleanup is separately classified.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tempfile


FORBIDDEN = (b"CAGENTS_AGENT_AUTHORITY", b"CAGENTS_AGENT_TOKEN", b"CAGENTS_SESSION_TOKEN")
HELPER_ENV = "CAGENTS_TEST_RETAINED_ACTOR_HELPER"
MANIFEST = "scripts/ci/fixture-host-qualification.json"
CONTROL_FILES = (
    ".github/workflows/fixture-host-qualification.yml",
    "scripts/ci/fixture-host-qualification.py",
    MANIFEST,
    "scripts/ci/observe-command-owned-fixtures.py",
)


def require_natural_user(helper_allowed=False):
    # Byte-key membership rejects empty and non-Unicode credentials too. No
    # credential value is read, copied, printed, removed or replaced.
    if any(name in os.environb for name in FORBIDDEN):
        raise RuntimeError("Inherited cagents authority is present; invocation refused")
    allowed = {HELPER_ENV.encode()} if helper_allowed else set()
    if any(name.startswith(b"CAGENTS_") and name not in allowed for name in os.environb):
        raise RuntimeError("Unexpected inherited cagents fixture context; invocation refused")
    if os.environ.get("GITHUB_ACTIONS") != "true" or os.environ.get("RUNNER_ENVIRONMENT") != "github-hosted":
        raise RuntimeError("Only an actual GitHub-hosted invocation is admitted")


def git(*args):
    return subprocess.check_output(["git", *args], text=True).strip()


def digest(path):
    checksum = hashlib.sha256()
    with Path(path).open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            checksum.update(chunk)
    return checksum.hexdigest()


def write_json(path, data):
    path.write_text(json.dumps(data, indent=2) + "\n")


def source_identity():
    return {"head": git("rev-parse", "HEAD"), "tree": git("rev-parse", "HEAD^{tree}"),
            "status": git("status", "--porcelain")}


def exact_source(product):
    state = source_identity()
    if state != {"head": product["head"], "tree": product["tree"], "status": ""}:
        raise RuntimeError("Product source identity/cleanliness drift")
    for name, expected in product["source_sha256"].items():
        if digest(name) != expected:
            raise RuntimeError("Owning product source hash drift")
    return state


def check_bundle(bundle, manifest, control_pins=None):
    pins = manifest["control_sha256"] | (control_pins or {})
    for name, expected in pins.items():
        path = bundle / name
        if path.is_symlink() or digest(path) != expected:
            raise RuntimeError("Control bundle hash drift")


def prepare():
    # This is intentionally before any mkdir, tempfile, capture or fixture.
    require_natural_user()
    manifest = json.loads(Path(MANIFEST).read_text())
    state = source_identity()
    if state["head"] != os.environ["FIXTURE_CI_CONTROL_HEAD"] or state["status"]:
        raise RuntimeError("Unexpected control checkout")
    changed = git("diff", "--name-only", manifest["main"], "HEAD").splitlines()
    if sorted(changed) != sorted(CONTROL_FILES):
        raise RuntimeError("Unexpected control/product composition")
    subprocess.run(["git", "merge-base", "--is-ancestor", manifest["main"], "HEAD"], check=True)
    check_bundle(Path.cwd(), manifest)
    for product in manifest["products"]:
        if git("rev-parse", product["head"] + "^{tree}") != product["tree"]:
            raise RuntimeError("Product immutable tree mismatch")
        changed = git("diff", "--name-only", manifest["main"], product["head"]).splitlines()
        if sorted(changed) != sorted(product["changed_paths"]):
            raise RuntimeError("Unexpected product scope")
        subprocess.run(["git", "merge-base", "--is-ancestor", manifest["main"], product["head"]], check=True)
    evidence = Path(os.environ["FIXTURE_CI_EVIDENCE"])
    evidence.mkdir(mode=0o700)
    bundle = evidence / "control"
    for name in CONTROL_FILES:
        target = bundle / name
        target.parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(name, target)
    write_json(evidence / "control-source.json", {
        "source": state, "control_head": os.environ["FIXTURE_CI_CONTROL_HEAD"],
        "control_files_sha256": {name: digest(name) for name in CONTROL_FILES},
        "workflow_ref": os.environ.get("GITHUB_WORKFLOW_REF"),
        "workflow_sha": os.environ.get("GITHUB_WORKFLOW_SHA"),
        "natural_user_guard_passed": True, "authority_removed_or_replaced": False,
        "scope": "Focused existing fixture contracts only; no general/full CI or native/provider qualification",
    })


class Qualifier:
    def __init__(self, evidence, bundle, manifest, product, control_pins):
        self.evidence = evidence
        self.bundle = bundle
        self.manifest = manifest
        self.product = product
        self.control_pins = control_pins
        self.pins = {}
        self.results = []

    def integrity(self):
        require_natural_user(helper_allowed=True)
        check_bundle(self.bundle, self.manifest, self.control_pins)
        source = exact_source(self.product)
        if any(digest(path) != expected for path, expected in self.pins.items()):
            raise RuntimeError("Matched helper/test executable hash drift")
        return source

    def observed(self, label, command, seconds=180, *, build_only=False):
        before = self.integrity()
        phase = self.evidence / label
        observer = self.bundle / "scripts/ci/observe-command-owned-fixtures.py"
        argv = ["timeout", "--signal=TERM", "--kill-after=20s", str(seconds + 30) + "s",
                sys.executable, str(observer), "--output", str(phase), "--timeout", str(seconds),
                "--", *map(str, command)]
        # The corrected, pinned observer owns this command tree. GNU timeout
        # bounds the whole observer too; any timeout/assistance cannot pass.
        launch = subprocess.run(argv, capture_output=True)
        (self.evidence / (label + ".observer.stdout")).write_bytes(launch.stdout)
        (self.evidence / (label + ".observer.stderr")).write_bytes(launch.stderr)
        after = self.integrity()
        metadata_path = phase / "COMMAND.json"
        metadata = json.loads(metadata_path.read_text()) if metadata_path.exists() else {}
        phase.mkdir(exist_ok=True)
        cleanup = metadata.get("cleanup", {})
        initial = cleanup.get("initial_census", [])
        leader = metadata.get("leader", {}).get("pid")
        retained = [row for row in initial if row.get("pid") == leader]
        dead_leader = (len(retained) == 1 and retained[0].get("state") == "Z"
                       and isinstance(retained[0].get("start_ticks"), int) and retained[0]["start_ticks"] > 0
                       and retained[0].get("start_ticks") == metadata.get("leader", {}).get("start_ticks"))
        orphans = [row for row in initial if row.get("pid") != leader]
        # A compiler can leave an already-exited adopted child for our
        # subreaper. Reaping a proved zombie cannot terminate live work. This
        # exception is explicit at Cargo build/--no-run callsites only; actual
        # test parents AND exact listings still require the dead leader alone.
        dead_build_orphans = all(
            row.get("state") == "Z"
            and isinstance(row.get("start_ticks"), int) and row["start_ticks"] > 0
            and row.get("membership", {}).get("kind") in {"kernel-direct-child", "reserved-leader-session"}
            and row.get("start_ticks") == row.get("membership", {}).get("start_ticks")
            for row in orphans
        )
        unassisted = (metadata.get("exit_reason") == "exited" and dead_leader
                      and (dead_build_orphans if build_only else not orphans))
        closed = (cleanup.get("complete") is True and cleanup.get("remaining") == []
                  and cleanup.get("membership_uncertain") == [] and cleanup.get("leader_reaped") is True
                  and metadata.get("sampler_closed") is True)
        result = {"label": label, "argv": argv, "observer_exit": launch.returncode,
                  "command_exit": metadata.get("command_exit"), "source_before": before,
                  "source_after": after, "cleanup_complete": closed,
                  "command_scope": "build-only" if build_only else "original-parent-or-list",
                  "unassisted_command_closure": unassisted,
                  "already_dead_build_orphans_reaped": orphans if build_only else [],
                  "cleanup_classification": "Build-only mode permits reaping already-dead proved owned zombies; test parents/listings require the dead leader alone. No live member or assisted termination can qualify; observer signals remain recorded.",
                  "passed": launch.returncode == 0 and metadata.get("command_exit") == 0 and closed and unassisted}
        write_json(phase / "qualification.json", result)
        self.results.append(result)
        write_json(self.evidence / "commands.json", self.results)
        if not closed or not unassisted:
            raise RuntimeError("Custody/normal closure failed; stop before another product command")
        return result, phase

    def artifact(self, phase, name, test):
        rows = []
        for line in (phase / "stdout.log").read_text().splitlines():
            try:
                row = json.loads(line)
            except ValueError:
                continue
            if (row.get("reason") == "compiler-artifact" and row.get("executable")
                    and row.get("target", {}).get("name") == name
                    and row.get("profile", {}).get("test") is test):
                rows.append(row)
        if len(rows) != 1:
            raise RuntimeError("Cargo did not identify exactly one expected product artifact")
        row = rows[0]
        if Path(row["manifest_path"]).resolve() != Path.cwd() / "Cargo.toml":
            raise RuntimeError("Cargo artifact does not belong to the exact product package")
        kind = "lib" if test and name == "cagents" else "test" if test else "bin"
        if kind not in row["target"]["kind"]:
            raise RuntimeError("Cargo artifact target kind does not match the owning target")
        features = row["features"]
        if "host" not in features or any(x in features for x in ["ui", "integrated-browser", "full"]):
            raise RuntimeError("Artifact is not from the isolated HOST graph")
        path = Path(row["executable"]).resolve(strict=True)
        if not path.is_file() or not os.access(path, os.X_OK):
            raise RuntimeError("Cargo executable is unavailable")
        self.pins[path] = digest(path)
        identity = {"source": self.integrity(), "path": str(path), "sha256": self.pins[path],
                    "compiler_artifact": row}
        write_json(self.evidence / (name + ("-test" if test else "-helper") + ".json"), identity)
        return path

    def parent(self, label, executable, selector, temporary_parent):
        os.environ["TMPDIR"] = str(temporary_parent)
        listing, listed = self.observed(label + "-list", [executable, "--list", "--exact", selector], 15)
        names = [line.removesuffix(": test") for line in (listed / "stdout.log").read_text().splitlines()
                 if line.endswith(": test")]
        if not listing["passed"] or names != [selector]:
            raise RuntimeError("Exact original parent was not uniquely listed")
        result, phase = self.observed(label, [executable, "--exact", selector, "--test-threads=1",
                                                "--nocapture", "--color=never"])
        stdout = (phase / "stdout.log").read_text(errors="replace")
        summaries = re.findall(r"^test result: (?:ok|FAILED)\. (\d+) passed; (\d+) failed; (\d+) ignored;", stdout, re.M)
        # Nested isolation summaries remain visible but are not independent
        # selected roots. The last summary belongs to the original outer parent.
        summary = list(map(int, summaries[-1])) if summaries else None
        empty = not any(temporary_parent.iterdir())
        result.update(selector=selector, exact_listed_roots=names, summary_lines=summaries,
                      outer_parent_counts=summary, helper_directory_removed=empty)
        result["passed"] = result["passed"] and summary == [1, 0, 0] and empty
        write_json(phase / "qualification.json", result)
        write_json(self.evidence / "commands.json", self.results)
        if not empty:
            raise RuntimeError("Product fixture directory remains; stop before reuse")
        return result["passed"]


def run():
    require_natural_user()
    if sys.version_info[:3] != (3, 12, 3):
        raise RuntimeError("Pinned Python 3.12.3 is required")
    evidence = Path(os.environ["FIXTURE_CI_EVIDENCE"])
    bundle = evidence / "control"
    manifest = json.loads((bundle / MANIFEST).read_text())
    control = json.loads((evidence / "control-source.json").read_text())
    if source_identity() != control["source"] or control["control_head"] != os.environ["FIXTURE_CI_CONTROL_HEAD"]:
        raise RuntimeError("Control checkout changed before qualification")
    control_pins = control["control_files_sha256"]
    source_pins = {name: hashlib.sha256(subprocess.check_output(["git", "show", "HEAD:" + name])).hexdigest()
                   for name in CONTROL_FILES}
    if source_pins != control_pins:
        raise RuntimeError("Archived control identity differs from the exact control commit")
    check_bundle(bundle, manifest, control_pins)
    write_json(evidence / "toolchains.json", {
        "python": sys.version, "rustc": subprocess.check_output(["rustc", "--version"], text=True),
        "cargo": subprocess.check_output(["cargo", "--version"], text=True),
        "observer_sha256": manifest["observer_sha256"],
    })
    results = []
    for product in manifest["products"]:
        require_natural_user(helper_allowed=True)
        phase = evidence / ("pr" + str(product["pr"]))
        phase.mkdir(mode=0o700)
        switched = subprocess.run(["git", "checkout", "--detach", product["head"]], capture_output=True, check=True)
        (phase / "checkout.stdout").write_bytes(switched.stdout)
        (phase / "checkout.stderr").write_bytes(switched.stderr)
        write_json(phase / "product-source.json", exact_source(product))
        qualifier = Qualifier(phase, bundle, manifest, product, control_pins)
        # Short private paths avoid Unix socket limits. Their creation follows
        # the hosted natural-scope guard; no authority is stripped or fabricated.
        scratch = Path(tempfile.mkdtemp(prefix="ff", dir="/tmp"))
        runtime = scratch / "run"
        runtime.mkdir(mode=0o700)
        os.environ["XDG_RUNTIME_DIR"] = str(runtime)
        os.environ["TMPDIR"] = str(scratch)
        flags = ["--locked", "--no-default-features", "--features", "host"]
        built, helper_phase = qualifier.observed("helper-build", ["cargo", "build", *flags, "--bin", "cagentsd",
                                                                   "--message-format=json-render-diagnostics"], 1800, build_only=True)
        if not built["passed"]:
            results.append({"pr": product["pr"], "passed": False, "reason": "helper build failed"})
            write_json(evidence / "results.json", results)
            continue
        helper = qualifier.artifact(helper_phase, "cagentsd", False)
        os.environ[HELPER_ENV] = str(helper)
        compiled, test_phase = qualifier.observed("test-build", ["cargo", "test", *flags, *product["compile_targets"],
                                                                  "--no-run", "--message-format=json-render-diagnostics"], 1800, build_only=True)
        if not compiled["passed"]:
            results.append({"pr": product["pr"], "passed": False, "reason": "owning test build failed"})
            write_json(evidence / "results.json", results)
            continue
        executable = qualifier.artifact(test_phase, product["test_artifact"], True)
        if product["pr"] == 1855:
            # Compile the real ignored target without executing any provider arm.
            qualifier.artifact(test_phase, "codex_continuity_qualification", True)
            outer = scratch / "outer"
            outer.mkdir(mode=0o700)
            subprocess.run(["git", "-c", "core.hooksPath=/dev/null", "init", "--quiet", "--template=",
                            "--initial-branch=outer-fixture", str(outer)], check=True, timeout=15)
            parent = outer / "tmp"
            parent.mkdir(mode=0o700)
            observed_outer = subprocess.check_output(["git", "-C", str(parent), "rev-parse", "--show-toplevel"], text=True).strip()
            if Path(observed_outer).resolve() != outer:
                raise RuntimeError("Enclosing Git operand was not established")
            write_json(phase / "enclosing-git-operand.json", {"root": str(outer), "temporary_parent": str(parent),
                                                            "observed_top_level": observed_outer})
            runs = [qualifier.parent("parent-" + str(i), executable, selector, parent)
                    for i, selector in enumerate(product["parents"], 1)]
        else:
            physical = scratch / "physical"
            physical.mkdir(mode=0o700)
            alias = scratch / "alias"
            alias.symlink_to(physical, target_is_directory=True)
            write_json(phase / "matrix-paths.json", {"physical": str(physical), "alias": str(alias),
                                                    "alias_resolves_to_physical": alias.resolve() == physical})
            runs = [qualifier.parent("matrix-" + label, executable, product["matrix_parent"], parent)
                    for label, parent in [("physical", physical), ("alias", alias)]]
            runs.extend(qualifier.parent("control-" + str(i), executable, selector, physical)
                        for i, selector in enumerate(product["parents"], 1))
        results.append({"pr": product["pr"], "source": qualifier.integrity(), "passed": all(runs),
                        "expected_parent_executions": product["expected_parent_executions"],
                        "actual_parent_executions": len(runs), "original_parent_results": runs,
                        "scratch": str(scratch), "commands": qualifier.results})
        write_json(evidence / "results.json", results)
    passed = (len(results) == len(manifest["products"]) and all(row["passed"] for row in results)
              and all(row.get("actual_parent_executions") == row.get("expected_parent_executions") for row in results))
    write_json(evidence / "final.json", {"passed": passed, "control_head": control["control_head"],
                                        "products": results, "full_ci_claimed": False})
    if not passed:
        raise SystemExit("One or more required product checks failed; no failure is waived")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    modes = parser.add_mutually_exclusive_group(required=True)
    modes.add_argument("--prepare", action="store_true")
    modes.add_argument("--run", action="store_true")
    # No revisions, selectors or authority controls are dispatch inputs.
    args = parser.parse_args()
    if args.prepare:
        prepare()
    else:
        run()
