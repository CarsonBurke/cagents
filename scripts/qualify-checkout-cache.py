#!/usr/bin/env python3
"""Qualify owned source/stamp freshness across worktrees sharing a Cargo cache.

The tiny workspace uses this repository's actual Cargo config and build script.
It also models plain and generated local patches without compiling vendors.
It needs only Cargo, Git, and Python; no native UI dependencies or downloads.
"""

import argparse
import hashlib
import json
import os
from pathlib import Path
import subprocess
import tempfile
import tomllib


ROOT = Path(__file__).resolve().parent.parent
IDENTITY = "CAGENTS_CHECKOUT_ROOT"
ANCHOR = f'const _: Option<&str> = option_env!("{IDENTITY}");\n'
OLD_TIME = 946684800
VENDOR_BUILD_HOOKS = (
    "vendor/iced-core", "vendor/iced-runtime", "vendor/iced-winit",
    "vendor/libcosmic", "vendor/libsqlite3-sys",
)
VENDOR_SOURCE_HOOKS = (
    "vendor/winit-wayland", "vendor/iced-accessibility", "vendor/accesskit-unix",
    "vendor/accesskit-atspi-common", "vendor/harfrust", "vendor/cosmic-text",
    "vendor/smithay-clipboard",
    "vendor/iced-graphics",
)
ENV_WATCH = f'println!("cargo:rerun-if-env-changed={IDENTITY}");'


def guard_vendor_hooks():
    manifest = tomllib.loads((ROOT / "Cargo.toml").read_text())
    actual = {
        patch["path"]
        for source in manifest["patch"].values()
        for patch in source.values()
        if isinstance(patch, dict) and "path" in patch
    }
    assert actual == set(VENDOR_BUILD_HOOKS + VENDOR_SOURCE_HOOKS), actual
    for vendor in VENDOR_BUILD_HOOKS:
        source = (ROOT / vendor / "build.rs").read_text()
        assert ANCHOR.strip() in source, vendor
        # Require the watch before any branch, including SQLite's early return.
        main = source.split("fn main() {", 1)[1].lstrip()
        assert main.startswith(ENV_WATCH), vendor
        if vendor in ("vendor/libcosmic", "vendor/libsqlite3-sys"):
            assert 'env::var_os("CARGO_MANIFEST_DIR")' in source, vendor
            assert 'env!("CARGO_MANIFEST_DIR")' not in source, vendor
    for vendor in VENDOR_SOURCE_HOOKS:
        package = tomllib.loads((ROOT / vendor / "Cargo.toml").read_text())
        assert package["package"].get("build", True) is False or not (ROOT / vendor / "build.rs").exists(), vendor
        library = package.get("lib", {}).get("path", "src/lib.rs")
        assert ANCHOR.strip() in (ROOT / vendor / library).read_text(), vendor


def run(*arguments, cwd, env=None):
    result = subprocess.run(
        arguments, cwd=cwd, env=env, text=True, capture_output=True
    )
    if result.returncode:
        raise RuntimeError(f"{arguments[0]} failed in {cwd}:\n{result.stdout}\n{result.stderr}")
    return result.stdout.strip(), result.stderr


def write(path, text):
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(text)


def qualify(directory, fixed):
    lane = directory / ("fixed" if fixed else "control")
    checkout_a = lane / "a"
    checkout_b = lane / "b"
    dependency = lane / "dependency"
    target = lane / "target"
    checkout_a.mkdir(parents=True)
    write(dependency / "Cargo.toml", '[package]\nname="qualification-dependency"\nversion="0.1.0"\nedition="2021"\n')
    write(dependency / "src/lib.rs", 'pub fn value() -> &\'static str { "dependency" }\n')
    config = (ROOT / ".cargo/config.toml").read_text()
    if not fixed:
        config = config.split("\n[env]\n")[0]
    write(checkout_a / ".cargo/config.toml", config)
    write(checkout_a / "Cargo.toml", '''[workspace]
members=[".", "crates/ceditor", "crates/browser-import", "crates/cache-probe"]
resolver="2"
[package]
name="cagents"
version="0.1.3"
edition="2021"
[dependencies]
cagents-browser-import={path="crates/browser-import"}
qualification-dependency={path="../dependency"}
''')
    build = (ROOT / "build.rs").read_text()
    if not fixed:
        build = build.replace(ANCHOR, "")
        build = build.replace(f'    println!("cargo:rerun-if-env-changed={IDENTITY}");\n', "")
    # Vary the actual stamp-producing script body independently of its runtime
    # Git lookup. A rerun in B can report B's dirty state using A's stale body.
    directive = 'println!("cargo:rustc-env=CAGENTS_VERSION={version}");'
    assert build.count(directive) == 1
    build = build.replace(directive, directive.replace("{version}", "{version}.body-A"))
    write(checkout_a / "build.rs", build)
    anchor = ANCHOR if fixed else ""
    write(checkout_a / "src/lib.rs", anchor + '''pub const VERSION: &str = env!("CAGENTS_VERSION");
pub fn value() -> &'static str { "root-A" }
''')
    write(checkout_a / "src/main.rs", '''fn main() {
    println!("{}|{}|{}|{}", cagents::VERSION, cagents::value(),
        cagents_browser_import::value(), qualification_dependency::value());
}
''')
    write(checkout_a / "crates/browser-import/Cargo.toml", '[package]\nname="cagents-browser-import"\nversion="0.1.0"\nedition="2021"\n')
    write(checkout_a / "crates/browser-import/src/lib.rs", anchor + 'pub fn value() -> &\'static str { "import-A" }\n')
    write(checkout_a / "crates/ceditor/Cargo.toml", '[package]\nname="ceditor"\nversion="0.1.0"\nedition="2021"\n[dependencies]\ncagents={path="../.."}\n')
    write(checkout_a / "crates/ceditor/src/main.rs", anchor + 'fn main() { println!("{}|editor-A", cagents::VERSION); }\n')
    # The observer alone is forced to relink even in the broken control. This
    # prevents a stale executable from masquerading as proof of stale dependencies.
    write(checkout_a / "crates/cache-probe/Cargo.toml", '''[package]
name="cache-probe"
version="0.1.0"
edition="2021"
[dependencies]
qualification-plain="0.1.0"
qualification-generated="0.1.0"
''')
    write(checkout_a / "crates/cache-probe/build.rs", '''fn main() {
    println!("cargo:rerun-if-env-changed=QUALIFICATION_PROBE_CHECKOUT");
    println!("cargo:rustc-env=QUALIFICATION_OBSERVER_CHECKOUT={}",
        std::env::var("QUALIFICATION_PROBE_CHECKOUT").unwrap());
}
''')
    write(checkout_a / "crates/cache-probe/src/main.rs", '''fn main() {
    println!("{}|{}|{}|{}", env!("QUALIFICATION_OBSERVER_CHECKOUT"), qualification_plain::value(),
        qualification_generated::source(), qualification_generated::generated());
}
''')
    with (checkout_a / "Cargo.toml").open("a") as manifest:
        manifest.write('''
[patch.crates-io]
qualification-plain={path="vendor/plain"}
qualification-generated={path="vendor/generated"}
''')
    for name in ("plain", "generated"):
        write(checkout_a / f"vendor/{name}/Cargo.toml", f'[package]\nname="qualification-{name}"\nversion="0.1.0"\nedition="2021"\n')
    write(checkout_a / "vendor/plain/src/lib.rs", anchor + 'pub fn value() -> &\'static str { "plain-A" }\n')
    # No source anchor here: the unchanged script watch must independently
    # invalidate both Rust source and generated output with old input mtimes.
    write(checkout_a / "vendor/generated/src/lib.rs", '''include!(concat!(env!("OUT_DIR"), "/generated.rs"));
pub fn source() -> &'static str { "generated-source-A" }
pub fn generated() -> &'static str { GENERATED }
''')
    write(checkout_a / "vendor/generated/input.txt", "generated-output-A")
    watch = ENV_WATCH if fixed else ""
    write(checkout_a / "vendor/generated/build.rs", '''fn main() {
    WATCH
    println!("cargo:rerun-if-changed=input.txt");
    let input = std::fs::read_to_string("input.txt").unwrap();
    let output = std::path::PathBuf::from(std::env::var_os("OUT_DIR").unwrap()).join("generated.rs");
    std::fs::write(output, format!("const GENERATED: &str = {:?};", input)).unwrap();
}
'''.replace("WATCH", watch))
    write(checkout_a / ".gitignore", "/target/\n")
    run("git", "init", "-q", cwd=checkout_a)
    run("git", "add", ".", cwd=checkout_a)
    run("git", "-c", "user.name=Cache qualification", "-c", "user.email=cache-fixture@invalid", "commit", "-qm", "Fixture baseline", cwd=checkout_a)
    run("git", "worktree", "add", "--detach", str(checkout_b), "HEAD", cwd=checkout_a)
    # Both worktrees have the same HEAD. Only B's dirty source differs, and its
    # timestamps intentionally cannot invalidate A's newer cached compilation.
    for relative in ("build.rs", "src/lib.rs", "crates/browser-import/src/lib.rs", "crates/ceditor/src/main.rs",
                     "vendor/plain/src/lib.rs", "vendor/generated/src/lib.rs", "vendor/generated/input.txt"):
        path = checkout_b / relative
        path.write_text(path.read_text().replace("-A", "-B"))
    for checkout in (checkout_a, checkout_b):
        tracked, _ = run("git", "ls-files", cwd=checkout)
        for relative in tracked.splitlines():
            os.utime(checkout / relative, (OLD_TIME, OLD_TIME))
        # Refresh Git's stat cache before Cargo records the watched index mtime.
        # Otherwise build.rs's first status query can itself dirty that watch.
        run("git", "status", "--porcelain", "--untracked-files=no", cwd=checkout)
    env = os.environ.copy()
    env.pop("CAGENTS_BUILD_METADATA", None)
    env.update(CARGO_TARGET_DIR=str(target), CARGO_BUILD_BUILD_DIR=str(lane / "build"), CARGO_BUILD_JOBS="2")
    # The configured force=true must defeat an inherited checkout identity.
    env[IDENTITY] = "deliberately-stale-inherited-value"
    sha, _ = run("git", "rev-parse", "--short", "HEAD", cwd=checkout_a)
    expected_stamp = f"0.1.3+local.{sha}"
    dependency_artifacts = None
    outcomes = []
    expected_outcomes = []
    patch_outcomes = []
    generated_directives = None
    checkouts = (checkout_a, checkout_b, checkout_a) + ((checkout_a,) if fixed else ())
    for step, checkout in enumerate(checkouts):
        if step == 3:
            # An env watch disables Cargo's default package-wide scan. Prove
            # that the explicit input watch still handles same-checkout edits,
            # independently of a checkout-identity change. Keep Rust source old.
            write(checkout / "vendor/generated/input.txt", "generated-output-A-edited")
        env["QUALIFICATION_PROBE_CHECKOUT"] = str(checkout)
        _, log = run("cargo", "build", "--offline", "--workspace", "--verbose", cwd=checkout, env=env)
        write(lane / f"build-{step}.log", log)
        root, _ = run(str(target / "debug/cagents"), cwd=checkout)
        editor, _ = run(str(target / "debug/ceditor"), cwd=checkout)
        probe, _ = run(str(target / "debug/cache-probe"), cwd=checkout)
        observed_checkout, probe = probe.split("|", 1)
        assert observed_checkout == str(checkout), ("observer executable was stale", checkout, observed_checkout)
        letter = "B" if step == 1 else "A"
        expected_patch = f"plain-{letter}|generated-source-{letter}|generated-output-{letter}"
        if step == 3:
            expected_patch = "plain-A|generated-source-A|generated-output-A-edited"
        patch_outcomes.append(probe)
        if fixed:
            assert probe == expected_patch, (checkout, expected_patch, probe)
        elif step == 1:
            assert probe == "plain-A|generated-source-A|generated-output-A", ("control patch dependencies were not stale", probe)
        elif step == 2:
            assert probe == "plain-A|generated-source-A|generated-output-A", probe
        # Rerunning the fixed generator changes OUT_DIR contents while keeping
        # its emitted cfg/link/watch directives byte-identical across checkouts.
        # Cargo's older flat layout and pinned nightly's crate/hash/run layout
        # are the only searches; do not recursively inventory the target tree.
        output_patterns = (
            "debug/build/qualification-generated-*/output",
            "debug/build/qualification-generated/*/run/output",
            "debug/build/qualification_generated/*/run/output",
            "debug/build/qualification-generated/*/run/stdout",
            "debug/build/qualification_generated/*/run/stdout",
        )
        directives = [p.read_bytes() for base in (target, lane / "build")
                      for pattern in output_patterns for p in base.glob(pattern)]
        assert directives, "missing generated build-script output in known Cargo layouts"
        if generated_directives is None:
            generated_directives = directives[0]
        assert all(value == generated_directives for value in directives), "generator directives changed"
        stamp = expected_stamp + (".dirty" if step == 1 else "") + f".body-{letter}"
        expected = (f"{stamp}|root-{letter}|import-{letter}|dependency", f"{stamp}|editor-{letter}")
        actual = (root, editor)
        if step < 3:
            outcomes.append(actual)
            expected_outcomes.append(expected)
            if fixed:
                assert actual == expected, (checkout, expected, actual)
        artifacts = {
            str(p.relative_to(lane)): p.stat().st_mtime_ns
            for base in (target, lane / "build")
            for p in base.rglob("libqualification_dependency-*.rlib")
        }
        assert artifacts, "missing dependency artifact"
        if dependency_artifacts is not None:
            assert artifacts == dependency_artifacts, "checkout identity invalidated an unowned dependency"
            assert "Fresh qualification-dependency" in log, log
        dependency_artifacts = artifacts
        print(f"{lane.name} {letter}{'-input-edit' if step == 3 else ''}: {root}; {editor}; {probe}")
    if not fixed:
        assert outcomes[1] != expected_outcomes[1], "control did not reproduce stale checkout reuse on this Cargo version"
    return {"owned": outcomes, "patched": patch_outcomes}


def qualify_manifest_paths(directory, fixed, *, body_identity=None, historical=False):
    """Distinguish current script inputs from an executable's baked-in paths."""
    lane = directory / ("body-historical-bootstrap" if historical else
                        (("body-anchored" if body_identity else "body-watch-only") if body_identity is not None
                         else ("metadata-fixed" if fixed else "metadata-control")))
    checkout_a = lane / "a"
    checkout_b = lane / "b"
    target = lane / "target"
    write(checkout_a / ".cargo/config.toml", (ROOT / ".cargo/config.toml").read_text())
    write(checkout_a / "Cargo.toml", '''[workspace]
members=[".", "vendor/path-source"]
resolver="2"
[package]
name="qualification-path-observer"
version="0.1.0"
edition="2021"
[dependencies]
qualification-path-source={path="vendor/path-source"}
qualification-independent={path="../independent"}
''')
    write(lane / "independent/Cargo.toml", '[package]\nname="qualification-independent"\nversion="0.1.0"\nedition="2021"\n')
    write(lane / "independent/src/lib.rs", 'pub fn independent() {}\n')
    write(checkout_a / "build.rs", '''fn main() {
    println!("cargo:rerun-if-env-changed=QUALIFICATION_PROBE_CHECKOUT");
    println!("cargo:rustc-env=QUALIFICATION_OBSERVER_CHECKOUT={}",
        std::env::var("QUALIFICATION_PROBE_CHECKOUT").unwrap());
    println!("cargo:rustc-env=QUALIFICATION_DEP_INCLUDE={}",
        std::env::var("DEP_QUALIFICATION_PATHS_INCLUDE").unwrap());
}
''')
    write(checkout_a / "src/main.rs", '''fn main() {
    println!("{}|{}|{}", env!("QUALIFICATION_OBSERVER_CHECKOUT"),
        qualification_path_source::input(), env!("QUALIFICATION_DEP_INCLUDE"));
}
''')
    write(checkout_a / "vendor/path-source/Cargo.toml", '''[package]
name="qualification-path-source"
version="0.1.0"
edition="2021"
links="qualification_paths"
''')
    write(checkout_a / "vendor/path-source/src/lib.rs", '''pub fn input() -> &'static str {
    env!("QUALIFICATION_GENERATED_INPUT")
}
''')
    manifest_dir = ('std::env::var_os("CARGO_MANIFEST_DIR").unwrap()' if fixed
                    else 'env!("CARGO_MANIFEST_DIR")')
    write(checkout_a / "vendor/path-source/build.rs", '''fn main() {
    WATCH
    println!("cargo:rerun-if-changed=input.txt");
    let input = std::fs::read_to_string("input.txt").unwrap();
    println!("cargo:rustc-env=QUALIFICATION_GENERATED_INPUT={input}");
    let manifest_dir = std::path::PathBuf::from(MANIFEST_LOOKUP);
    println!("cargo:include={}", manifest_dir.display());
}
'''.replace("WATCH", ENV_WATCH).replace("MANIFEST_LOOKUP", manifest_dir))
    if body_identity is not None:
        script = checkout_a / "vendor/path-source/build.rs"
        body = script.read_text().replace("manifest_dir.display()", 'manifest_dir.join("body-A").display()')
        if body_identity:
            body = ANCHOR + body
        write(script, body)
    write(checkout_a / "vendor/path-source/input.txt", "input-A")
    write(checkout_a / ".gitignore", "/target/\n")
    run("git", "init", "-q", cwd=checkout_a)
    run("git", "add", ".", cwd=checkout_a)
    run("git", "-c", "user.name=Cache qualification", "-c", "user.email=cache-fixture@invalid",
        "commit", "-qm", "Path metadata fixture", cwd=checkout_a)
    run("git", "worktree", "add", "--detach", str(checkout_b), "HEAD", cwd=checkout_a)
    write(checkout_b / "vendor/path-source/input.txt", "input-B")
    if body_identity is not None:
        script = checkout_b / "vendor/path-source/build.rs"
        body = script.read_text().replace("body-A", "body-B")
        if historical:
            # Historical checkout B predates the compiler anchor. A runtime
            # watch alone cannot force this executable to recompile on return.
            body = body.replace(ANCHOR, "")
        write(script, body)
    for checkout in (checkout_a, checkout_b):
        tracked, _ = run("git", "ls-files", cwd=checkout)
        for relative in tracked.splitlines():
            os.utime(checkout / relative, (OLD_TIME, OLD_TIME))
    env = os.environ.copy()
    env.update(CARGO_TARGET_DIR=str(target), CARGO_BUILD_BUILD_DIR=str(lane / "build"),
               CARGO_BUILD_JOBS="2", CAGENTS_CHECKOUT_ROOT="deliberately-stale-inherited-value")
    independent_artifacts = None
    bootstrap_observations = []
    script_compilations = []
    compiler_anchors = []
    checkouts = (checkout_a, checkout_b, checkout_a) + ((checkout_a,) if historical else ())
    for step, checkout in enumerate(checkouts):
        if step == 3:
            # This explicit package selection models the admitted maintenance
            # operation; never clean the whole shared cache or alter timestamps.
            _, clean_log = run("cargo", "clean", "--offline", "--package",
                               "qualification-path-source", cwd=checkout, env=env)
            write(lane / "bootstrap-clean.log", clean_log)
        env["QUALIFICATION_PROBE_CHECKOUT"] = str(checkout)
        _, log = run("cargo", "build", "--offline", "--workspace", "--verbose", cwd=checkout, env=env)
        write(lane / f"build-{step}.log", log)
        if historical:
            compilations = sum("--crate-name build_script_build" in line
                               and "vendor/path-source/build.rs" in line
                               for line in log.splitlines() if "Running `" in line)
            script_compilations.append(compilations)
            dep_infos = [path for base in (target, lane / "build")
                         for path in base.rglob("build_script_build*.d")
                         if "vendor/path-source/build.rs" in path.read_text()]
            assert len(dep_infos) == 1, ("ambiguous build-script dep-info", dep_infos)
            dep_info = dep_infos[0].read_text()
            write(lane / f"build-{step}.dep-info", dep_info)
            compiler_anchors.append(f"# env-dep:{IDENTITY}=" in dep_info)
        observed, _ = run(str(target / "debug/qualification-path-observer"), cwd=checkout)
        observer, input_value, include = observed.split("|")
        assert observer == str(checkout), ("metadata observer was stale", checkout, observer)
        assert input_value == ("input-B" if step == 1 else "input-A"), (checkout, input_value)
        expected_root = checkout if fixed else checkout_a
        expected_include = expected_root / "vendor/path-source"
        if body_identity is not None:
            # Watch-only control deliberately observes stale executable BODY
            # while relative input and runtime manifest path follow checkout B.
            expected_body = "body-B" if body_identity and (step == 1 or historical and step == 2) else "body-A"
            expected_include = expected_include / expected_body
        assert include == str(expected_include), (checkout, include)
        if not fixed and step == 1:
            assert include != str(checkout / "vendor/path-source"), "control did not leak its compiled manifest path"
        artifacts = {
            str(p.relative_to(lane)): (p.stat().st_mtime_ns, hashlib.sha256(p.read_bytes()).hexdigest())
            for base in (target, lane / "build")
            for p in base.rglob("libqualification_independent-*.rlib")
        }
        assert artifacts, "missing independent dependency artifact"
        if independent_artifacts is not None:
            assert artifacts == independent_artifacts, "package bootstrap invalidated unrelated dependency"
            assert "Fresh qualification-independent" in log, log
        independent_artifacts = artifacts
        bootstrap_observations.append(observed)
        print(f"{lane.name}: {observed}")
    if historical:
        assert script_compilations == [1, 1, 0, 1], script_compilations
        assert compiler_anchors == [True, False, False, True], compiler_anchors
        # Preserve all four results rather than calling stale return a pass.
        write(lane / "bootstrap-observations.json", json.dumps({
            "sequence": ["anchored-A", "unanchored-B", "stale-return-A", "package-bootstrap-A"],
            "observations": bootstrap_observations,
            "package": "qualification-path-source",
            "script_compilations": script_compilations,
            "compiler_checkout_anchors": compiler_anchors,
            "unrelated_artifacts": independent_artifacts,
        }, indent=2) + "\n")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--evidence", type=Path, help="retain fixture and Cargo logs in a new directory")
    args = parser.parse_args()
    config = tomllib.loads((ROOT / ".cargo/config.toml").read_text())
    assert config["env"][IDENTITY] == {"value": ".", "relative": True, "force": True}
    # Both compilation roots share the unconditional anchor in modules.rs.
    for source in ("src/lib.rs", "src/hot-ui.rs"):
        assert (ROOT / source).read_text().splitlines().count(
            'include!("modules.rs");'
        ) == 1, source
    for source in (
        "build.rs", "src/modules.rs", "crates/browser-import/src/lib.rs", "crates/ceditor/src/main.rs",
        "crates/iced-contracts/src/lib.rs",
    ):
        assert ANCHOR.strip() in (ROOT / source).read_text(), source
    guard_vendor_hooks()
    if args.evidence:
        directory = args.evidence.resolve()
        directory.mkdir(parents=True, exist_ok=False)
        qualify(directory, False)
        qualify(directory, True)
        qualify_manifest_paths(directory, False)
        qualify_manifest_paths(directory, True)
        qualify_manifest_paths(directory, True, body_identity=False)
        qualify_manifest_paths(directory, True, body_identity=True)
        qualify_manifest_paths(directory, True, body_identity=True, historical=True)
    else:
        with tempfile.TemporaryDirectory(prefix="cagents-cache-qualification-", dir="/var/tmp") as temporary:
            qualify(Path(temporary), False)
            qualify(Path(temporary), True)
            qualify_manifest_paths(Path(temporary), False)
            qualify_manifest_paths(Path(temporary), True)
            qualify_manifest_paths(Path(temporary), True, body_identity=False)
            qualify_manifest_paths(Path(temporary), True, body_identity=True)
            qualify_manifest_paths(Path(temporary), True, body_identity=True, historical=True)
    print("PASS: stale reuse reproduced; owned sources, stamps, patches and build-script path metadata follow A → B → A; same-checkout generated input stays live; external dependency remains fresh; watch-only stale script BODY reproduced and compile-anchor BODY follows A → B → A; historical unanchored producer is reproduced and package bootstrap restores A without cleaning unrelated dependencies")


if __name__ == "__main__":
    main()
