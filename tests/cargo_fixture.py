"""Real Cargo for test fixtures, apart from the user's Cargo home.

A fixture that runs real Cargo under the caller's Cargo home reads the user's
configuration, such as a `build-dir` that moves intermediates out of target/,
and shares the package-cache lock with every other build on this host. While
another build holds that lock, Cargo waits and prints "Blocking waiting for
file lock on package cache", which then reaches the output a fixture reads.
A private Cargo home has neither, and the tiny crates fixtures build need no
registry to fill it.
"""

import os
from pathlib import Path
import shutil
import subprocess


def user_cargo_home(environment=os.environ):
    """The Cargo home the caller's Cargo uses. Its bin/ holds rustup's proxies,
    which keep working under another Cargo home."""
    if environment.get("CARGO_HOME"):
        return Path(environment["CARGO_HOME"])
    return Path(environment.get("HOME") or Path.home()) / ".cargo"


def private_cargo_environment(home, environment=os.environ):
    """`environment` for real Cargo with `home` as its Cargo home, and without the
    caller's CARGO_* variables, which Cargo also reads as configuration."""
    return {name: value for name, value in environment.items()
            if not name.startswith("CARGO_")} | {"CARGO_HOME": str(home)}


def toolchain_installed(toolchain):
    """Whether rustup has `toolchain`, such as "1.100.0", installed here."""
    if shutil.which("rustup") is None:
        return False
    listed = subprocess.run(["rustup", "toolchain", "list"], capture_output=True, text=True)
    return any(line.startswith(f"{toolchain}-") for line in listed.stdout.splitlines())
