#!/usr/bin/python3
"""Prepare the matched daemon used by HOST unit tests, inside an admitted build.

Stable Cargo supplies CARGO_BIN_EXE_* to integration tests, not library tests,
and only for the tested package's own binaries. The daemon's integration tests
also run the `cagents` command line, the shared library's binary, which they
find beside cagentsd; it is built here with the daemon, so a run narrowed to
those tests never uses one older than the library.
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


def has_bin(package, name):
    return any(target["name"] == name and "bin" in target["kind"] for target in package.get("targets", []))


def is_selected(metadata, package, parsed):
    """Whether the command selects the package, as Cargo would."""
    requested = [value for name, value, _ in parsed if name in ("-p", "--package")]
    excluded = [value for name, value, _ in parsed if name == "--exclude"]
    name = package["name"]
    matches = lambda spec: spec in (package["id"], name, f"{name}@{package['version']}")
    if any(matches(spec) for spec in excluded):
        return False
    if requested:
        return any(matches(spec) for spec in requested)
    if any(option in ("--workspace", "--all") for option, _, _ in parsed):
        return True
    # Metadata's default members describes the workspace root. A member
    # invoked by --manifest-path or from its directory selects itself.
    manifest = next((value for option, value, _ in parsed if option == "--manifest-path"), None)
    invoked = Path(manifest).resolve() if manifest else next(
        (parent / "Cargo.toml" for parent in [Path.cwd(), *Path.cwd().parents]
         if (parent / "Cargo.toml").is_file()), None)
    if invoked != Path(metadata["workspace_root"]) / "Cargo.toml":
        return invoked == Path(package["manifest_path"])
    return package["id"] in metadata.get("workspace_default_members", [])


def enables_host(package, parsed):
    """Whether the command's features select the shared library's `host`."""
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
            return True
        for enabled in package.get("features", {}).get(feature, []):
            if enabled not in features:
                features.add(enabled)
                pending.append(enabled)
    return False


def selected_host(metadata, parsed):
    """The packages whose executables the HOST tests run, and whether the
    command tests the daemon's own package, or None when it tests no daemon.

    The daemon is its own package, cagents-host. Its tests run cagentsd and
    the command line beside it. The shared library's tests with `host` run
    cagentsd as a retained helper, so they also want it, built from the
    daemon's package. Before that split, cagentsd was the shared library's
    binary, built only with that library's `host` feature."""
    packages = metadata.get("packages", [])
    daemon = next((package for package in packages if has_bin(package, "cagentsd")), None)
    if daemon is None:
        return None
    if daemon["name"] == "cagents":
        if is_selected(metadata, daemon, parsed) and enables_host(daemon, parsed):
            return [daemon], False
        return None
    if is_selected(metadata, daemon, parsed):
        # The daemon's integration tests also drive the command line, found
        # beside cagentsd: built with it, it is never older than the library.
        cli = next((package for package in packages if package["name"] == "cagents"
                    and has_bin(package, "cagents")), None)
        return ([daemon, cli] if cli else [daemon]), True
    library = next((package for package in packages if package["name"] == "cagents"), None)
    if library and is_selected(metadata, library, parsed) and enables_host(library, parsed):
        return [daemon], False
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
                               stderr=subprocess.DEVNULL, close_fds=False)
    if described.returncode:
        return None  # Cargo itself diagnoses an invalid original command.
    try:
        selected = selected_host(json.loads(described.stdout), parsed)
    except (ValueError, KeyError, TypeError):
        return None
    if selected is None:
        return None
    packages, tests_daemon = selected
    build = [word for name, _, words in parsed
             if name.startswith("-") and name not in SELECTION for word in words]
    if not tests_daemon and packages[0]["name"] != "cagents":
        # The library's features name none of the daemon's, which always
        # builds the library with `host`.
        build = [word for name, _, words in parsed if name.startswith("-")
                 and name not in SELECTION | {"--features", "-F", "--all-features"}
                 for word in words]
    if not any(name in ("--profile", "--release", "-r") for name, _, _ in parsed):
        build += ["--profile", "test"]
    # A caller that reads the tests' messages as JSON, such as scripts/cargo-lane,
    # judges the warnings of what the tests compile, and would get this side
    # build's again, unfiltered, on standard error: it gets only this build's
    # errors and other diagnostics that are not warnings.
    formats = ",".join(value or "" for name, value, _ in parsed if name == "--message-format")
    errors_only = any(part.strip().startswith("json") for part in formats.split(","))
    selection = [word for package in packages for word in ("--package", package["id"])]
    selection += ["--bin", "cagentsd", *(["--bin", "cagents"] if len(packages) > 1 else [])]
    if tests_daemon and any("test-support" in package.get("features", {})
                            for package in packages if package["name"] == "cagents"):
        # The test build enables the daemon's dev-dependency on the library's
        # `test-support`, which Cargo unifies into every unit it builds, so
        # the helper runs the library's test-support paths as the tests' own
        # cagentsd does. Other dev-dependency features, such as tokio's
        # test-util, still differ, so Cargo builds the daemon a second time.
        build += ["--features", "cagents/test-support"]
    command = [cargo, *toolchain, "build", *build, *selection,
               "--message-format=" + ("json" if errors_only else "json-render-diagnostics")]
    built = "cagentsd and its cagents" if len(packages) > 1 else "cagentsd"
    print(f"cargo-gate: building the matched {built} helper for HOST tests", file=sys.stderr)
    artifacts = CargoArtifacts()
    # Cargo keeps the jobserver descriptors MAKEFLAGS names.
    with subprocess.Popen(command, stdout=subprocess.PIPE, text=True, close_fds=False) as child:
        for line in child.stdout:
            message = artifacts.read(line, diagnostics=None if errors_only else sys.stderr)
            if not errors_only:
                continue
            if message is None:
                sys.stderr.write(line)
                continue
            diagnostic = message.get("message") if message.get("reason") == "compiler-message" else None
            if isinstance(diagnostic, dict) and diagnostic.get("level") != "warning":
                rendered = diagnostic.get("rendered")
                sys.stderr.write(rendered if isinstance(rendered, str) and rendered
                                 else f"{diagnostic.get('message', '')}\n")
        status = child.wait()
    if status:
        raise SystemExit(status if status > 0 else 128 - status)
    return str(artifacts.executable("cagentsd"))


def main():
    arguments = sys.argv[1:]
    if not arguments:
        raise SystemExit("usage: cargo-host-test-helper.py CARGO COMMAND...")
    cargo, arguments = arguments[0], arguments[1:]
    helper = prepare(cargo, arguments)
    if helper:
        print(helper)


if __name__ == "__main__":
    try:
        main()
    except (OSError, CargoArtifactError) as error:
        raise SystemExit(f"cargo-gate: could not prepare the HOST test helper: {error}")
