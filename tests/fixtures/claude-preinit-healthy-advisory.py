#!/usr/bin/python3 -ISB
"""Fixed offline preinit entrypoint; no child process or recipe flag change."""
from pathlib import Path
import runpy
import sys


if __name__ == "__main__":
    # The original generated suffix is forwarded without filtering or rewriting.
    sys.argv[1:1] = ["--fixture-scenario", 'healthy-advisory']
    runpy.run_path(
        str(Path(__file__).with_name("claude-preinit-contract.py")),
        run_name="__main__",
    )
