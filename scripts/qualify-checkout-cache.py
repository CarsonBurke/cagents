#!/usr/bin/env python3
"""Qualify owned source/stamp freshness across worktrees sharing a Cargo cache.

The tiny workspace uses this repository's actual Cargo config and build script.
It needs only Cargo, Git, and Python; no native UI dependencies or downloads.
"""

import argparse
import os
from pathlib import Path
import subprocess
import tempfile
import tomllib


ROOT = Path(__file__).resolve().parent.parent
IDENTITY = "CAGENTS_CHECKOUT_ROOT"
ANCHOR = f'const _: Option<&str> = option_env!("{IDENTITY}");\n'
OLD_TIME = 946684800


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
members=[".", "crates/ceditor", "crates/browser-import"]
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
        build = build.replace(f'    println!("cargo:rerun-if-env-changed={IDENTITY}");\n', "")
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
    write(checkout_a / ".gitignore", "/target/\n")
    run("git", "init", "-q", cwd=checkout_a)
    run("git", "add", ".", cwd=checkout_a)
    run("git", "-c", "user.name=Cache qualification", "-c", "user.email=cache-fixture@invalid", "commit", "-qm", "Fixture baseline", cwd=checkout_a)
    run("git", "worktree", "add", "--detach", str(checkout_b), "HEAD", cwd=checkout_a)
    # Both worktrees have the same HEAD. Only B's dirty source differs, and its
    # timestamps intentionally cannot invalidate A's newer cached compilation.
    for relative in ("src/lib.rs", "crates/browser-import/src/lib.rs", "crates/ceditor/src/main.rs"):
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
    for step, checkout in enumerate((checkout_a, checkout_b, checkout_a)):
        _, log = run("cargo", "build", "--offline", "--workspace", "--verbose", cwd=checkout, env=env)
        write(lane / f"build-{step}.log", log)
        root, _ = run(str(target / "debug/cagents"), cwd=checkout)
        editor, _ = run(str(target / "debug/ceditor"), cwd=checkout)
        letter = "B" if step == 1 else "A"
        stamp = expected_stamp + (".dirty" if step == 1 else "")
        expected = (f"{stamp}|root-{letter}|import-{letter}|dependency", f"{stamp}|editor-{letter}")
        actual = (root, editor)
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
        print(f"{lane.name} {letter}: {root}; {editor}")
    if not fixed:
        assert outcomes[1] != expected_outcomes[1], "control did not reproduce stale checkout reuse on this Cargo version"
    return outcomes


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--evidence", type=Path, help="retain fixture and Cargo logs in a new directory")
    args = parser.parse_args()
    config = tomllib.loads((ROOT / ".cargo/config.toml").read_text())
    assert config["env"][IDENTITY] == {"value": ".", "relative": True, "force": True}
    for source in ("src/lib.rs", "crates/browser-import/src/lib.rs", "crates/ceditor/src/main.rs"):
        assert ANCHOR.strip() in (ROOT / source).read_text(), source
    if args.evidence:
        directory = args.evidence.resolve()
        directory.mkdir(parents=True, exist_ok=False)
        qualify(directory, False)
        qualify(directory, True)
    else:
        with tempfile.TemporaryDirectory(prefix="cagents-cache-qualification-", dir="/var/tmp") as temporary:
            qualify(Path(temporary), False)
            qualify(Path(temporary), True)
    print("PASS: stale reuse reproduced; all owned sources and stamps follow A → B → A; dependency remains fresh")


if __name__ == "__main__":
    main()
