#!/usr/bin/python3
"""Prepare the matched daemon used by HOST unit tests, inside an admitted build.

Stable Cargo supplies CARGO_BIN_EXE_* to integration tests, not library tests.
The gate calls this before its test build; the offload runs it in the slot before
Cargo. Neither a guessed target path nor an older daemon is a valid substitute.
"""
import json
import os
from pathlib import Path
import subprocess
import sys

sys.path.insert(0, str(Path(__file__).resolve().parent))
from cargo_artifacts import CargoArtifactError, CargoArtifacts


HELPER = "CAGENTS_TEST_RETAINED_ACTOR_HELPER"
VALUED = {"-p", "--package", "--bin", "--example", "--test", "--bench", "-F", "--features",
          "-j", "--jobs", "--profile", "--target", "--target-dir", "--manifest-path",
          "--message-format", "--color", "--config", "-Z", "--lockfile-path", "--exclude"}
SELECTION = {"--lib", "--bins", "--bin", "--examples", "--example", "--tests", "--test",
             "--benches", "--bench", "--all-targets", "-p", "--package", "--workspace",
             "--all", "--exclude", "--no-run", "--no-fail-fast", "--message-format"}
METADATA = {"--manifest-path", "--config", "--locked", "--offline", "--frozen",
            "--lockfile-path"}


def options(arguments):
    """Keep option/value pairs intact, including attached short options."""
    words = iter(arguments)
    for word in words:
        if word == "--":
            break
        if word.startswith("--") and "=" in word:
            name, value = word.split("=", 1)
            yield name, value, [word]
        elif word in VALUED:
            value = next(words, None)
            yield word, value, [word] + ([value] if value is not None else [])
        elif len(word) > 2 and word[:2] in VALUED:
            yield word[:2], word[2:], [word]
        else:
            yield word, None, [word]


def selected_host(metadata, parsed):
    packages = metadata.get("packages", [])
    package = next((package for package in packages if package["name"] == "cagents"
                    and any(target["name"] == "cagentsd" and "bin" in target["kind"]
                            for target in package.get("targets", []))), None)
    if package is None:
        return None
    requested = [value for name, value, _ in parsed if name in ("-p", "--package")]
    excluded = [value for name, value, _ in parsed if name == "--exclude"]
    matches = lambda spec: spec in (package["id"], "cagents", "cagents@" + package["version"])
    if any(matches(spec) for spec in excluded):
        return None
    if requested:
        if not any(matches(spec) for spec in requested):
            return None
    elif not any(name in ("--workspace", "--all") for name, _, _ in parsed):
        # Metadata's default members describes the workspace root. A member
        # invoked by --manifest-path or from its directory selects itself.
        manifest = next((value for name, value, _ in parsed if name == "--manifest-path"), None)
        invoked = Path(manifest).resolve() if manifest else next(
            (parent / "Cargo.toml" for parent in [Path.cwd(), *Path.cwd().parents]
             if (parent / "Cargo.toml").is_file()), None)
        if invoked != Path(metadata["workspace_root"]) / "Cargo.toml":
            if invoked != Path(package["manifest_path"]):
                return None
        elif package["id"] not in metadata.get("workspace_default_members", []):
            return None
    features = set()
    if not any(name == "--no-default-features" for name, _, _ in parsed):
        features.add("default")
    for name, value, _ in parsed:
        if name in ("--features", "-F") and value:
            features.update(value.replace(",", " ").split())
        if name == "--all-features":
            features.update(package.get("features", {}))
    pending = list(features)
    while pending:
        feature = pending.pop()
        feature = feature.removeprefix("cagents/")
        if feature == "host":
            return package
        for enabled in package.get("features", {}).get(feature, []):
            if enabled not in features:
                features.add(enabled)
                pending.append(enabled)
    return None


def prepare(cargo, arguments):
    if os.environ.get(HELPER) or not arguments:
        return None
    toolchain = []
    if arguments[0].startswith("+"):
        toolchain, arguments = arguments[:1], arguments[1:]
    if not arguments or arguments[0] not in ("test", "t"):
        return None
    parsed = list(options(arguments[1:]))
    if any(name in ("--doc", "--no-run", "--help", "-h") for name, _, _ in parsed):
        return None
    metadata_args = [word for name, _, words in parsed if name in METADATA for word in words]
    described = subprocess.run([cargo, *toolchain, "metadata", "--format-version", "1",
                                "--no-deps", *metadata_args], stdout=subprocess.PIPE,
                               stderr=subprocess.DEVNULL)
    if described.returncode:
        return None  # Cargo itself diagnoses an invalid original command.
    try:
        package = selected_host(json.loads(described.stdout), parsed)
    except (ValueError, KeyError, TypeError):
        return None
    if package is None:
        return None
    build = [word for name, _, words in parsed
             if name.startswith("-") and name not in SELECTION for word in words]
    if not any(name in ("--profile", "--release", "-r") for name, _, _ in parsed):
        build += ["--profile", "test"]
    command = [cargo, *toolchain, "build", *build, "--package", package["id"],
               "--bin", "cagentsd", "--message-format=json-render-diagnostics"]
    print("cargo-gate: building the matched cagentsd helper for HOST tests", file=sys.stderr)
    artifacts = CargoArtifacts()
    with subprocess.Popen(command, stdout=subprocess.PIPE, text=True) as child:
        for line in child.stdout:
            artifacts.read(line, diagnostics=sys.stderr)
        status = child.wait()
    if status:
        raise SystemExit(status if status > 0 else 128 - status)
    return str(artifacts.executable("cagentsd"))


def main():
    arguments = sys.argv[1:]
    run = bool(arguments and arguments[0] == "--run")
    if run:
        arguments.pop(0)
    if not arguments:
        raise SystemExit("usage: cargo-host-test-helper.py [--run] CARGO COMMAND...")
    cargo, arguments = arguments[0], arguments[1:]
    helper = prepare(cargo, arguments)
    if run:
        if helper:
            os.environ[HELPER] = helper
        os.execvp(cargo, [cargo, *arguments])
    elif helper:
        print(helper)


if __name__ == "__main__":
    try:
        main()
    except (OSError, CargoArtifactError) as error:
        raise SystemExit(f"cargo-gate: could not prepare the HOST test helper: {error}")
